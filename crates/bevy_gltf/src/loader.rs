use crate::{
    vertex_attributes::convert_attribute, Gltf, GltfAssetLabel, GltfExtras, GltfMaterialExtras,
    GltfMaterialName, GltfMeshExtras, GltfNode, GltfSceneExtras, GltfSkin,
};

use alloc::collections::VecDeque;
use bevy_asset::{
    io::Reader, AssetLoadError, AssetLoader, Handle, LoadContext, ReadAssetBytesError,
};
use bevy_color::{Color, LinearRgba};
use bevy_core_pipeline::prelude::Camera3d;
use bevy_ecs::{
    entity::{hash_map::EntityHashMap, Entity},
    hierarchy::ChildSpawner,
    name::Name,
    world::World,
};
use bevy_image::{
    CompressedImageFormats, Image, ImageAddressMode, ImageFilterMode, ImageLoaderSettings,
    ImageSampler, ImageSamplerDescriptor, ImageType, TextureError,
};
use bevy_math::{Affine2, Mat4, Vec3};
use bevy_pbr::{
    DirectionalLight, MeshMaterial3d, PointLight, SpotLight, StandardMaterial, UvChannel,
    MAX_JOINTS,
};
use bevy_platform_support::collections::{HashMap, HashSet};
use bevy_render::{
    alpha::AlphaMode,
    camera::{Camera, OrthographicProjection, PerspectiveProjection, Projection, ScalingMode},
    mesh::{
        morph::{MeshMorphWeights, MorphAttributes, MorphTargetImage, MorphWeights},
        skinning::{SkinnedMesh, SkinnedMeshInverseBindposes},
        Indices, Mesh, Mesh3d, MeshVertexAttribute, VertexAttributeValues,
    },
    primitives::Aabb,
    render_asset::RenderAssetUsages,
    render_resource::{Face, PrimitiveTopology},
    view::Visibility,
};
use bevy_scene::Scene;
#[cfg(not(target_arch = "wasm32"))]
use bevy_tasks::IoTaskPool;
use bevy_transform::components::Transform;
use gltf::{
    accessor::Iter,
    image::Source,
    json,
    mesh::{util::ReadIndices, Mode},
    texture::{Info, MagFilter, MinFilter, TextureTransform, WrappingMode},
    Document, Material, Node, Primitive, Semantic,
};
use serde::{Deserialize, Serialize};
#[cfg(any(
    feature = "pbr_specular_textures",
    feature = "pbr_multi_layer_material_textures"
))]
use serde_json::Map;
use serde_json::{value, Value};
use std::{
    io::Error,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tracing::{error, info_span, warn};
#[cfg(feature = "bevy_animation")]
use {
    bevy_animation::{prelude::*, AnimationTarget, AnimationTargetId},
    smallvec::SmallVec,
};

/// An error that occurs when loading a glTF file.
#[derive(Error, Debug)]
pub enum GltfError {
    /// Unsupported primitive mode.
    #[error("unsupported primitive mode")]
    UnsupportedPrimitive {
        /// The primitive mode.
        mode: Mode,
    },
    /// Invalid glTF file.
    #[error("invalid glTF file: {0}")]
    Gltf(#[from] gltf::Error),
    /// Binary blob is missing.
    #[error("binary blob is missing")]
    MissingBlob,
    /// Decoding the base64 mesh data failed.
    #[error("failed to decode base64 mesh data")]
    Base64Decode(#[from] base64::DecodeError),
    /// Unsupported buffer format.
    #[error("unsupported buffer format")]
    BufferFormatUnsupported,
    /// Invalid image mime type.
    #[error("invalid image mime type: {0}")]
    #[from(ignore)]
    InvalidImageMimeType(String),
    /// Error when loading a texture. Might be due to a disabled image file format feature.
    #[error("You may need to add the feature for the file format: {0}")]
    ImageError(#[from] TextureError),
    /// Failed to read bytes from an asset path.
    #[error("failed to read bytes from an asset path: {0}")]
    ReadAssetBytesError(#[from] ReadAssetBytesError),
    /// Failed to load asset from an asset path.
    #[error("failed to load asset from an asset path: {0}")]
    AssetLoadError(#[from] AssetLoadError),
    /// Missing sampler for an animation.
    #[error("Missing sampler for animation {0}")]
    #[from(ignore)]
    MissingAnimationSampler(usize),
    /// Failed to generate tangents.
    #[error("failed to generate tangents: {0}")]
    GenerateTangentsError(#[from] bevy_render::mesh::GenerateTangentsError),
    /// Failed to generate morph targets.
    #[error("failed to generate morph targets: {0}")]
    MorphTarget(#[from] bevy_render::mesh::morph::MorphBuildError),
    /// Circular children in Nodes
    #[error("GLTF model must be a tree, found cycle instead at node indices: {0:?}")]
    #[from(ignore)]
    CircularChildren(String),
    /// Failed to load a file.
    #[error("failed to load file: {0}")]
    Io(#[from] Error),
}

/// Loads glTF files with all of their data as their corresponding bevy representations.
pub struct GltfLoader {
    /// List of compressed image formats handled by the loader.
    pub supported_compressed_formats: CompressedImageFormats,
    /// Custom vertex attributes that will be recognized when loading a glTF file.
    ///
    /// Keys must be the attribute names as found in the glTF data, which must start with an underscore.
    /// See [this section of the glTF specification](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#meshes-overview)
    /// for additional details on custom attributes.
    pub custom_vertex_attributes: HashMap<Box<str>, MeshVertexAttribute>,
}

/// Specifies optional settings for processing gltfs at load time. By default, all recognized contents of
/// the gltf will be loaded.
///
/// # Example
///
/// To load a gltf but exclude the cameras, replace a call to `asset_server.load("my.gltf")` with
/// ```no_run
/// # use bevy_asset::{AssetServer, Handle};
/// # use bevy_gltf::*;
/// # let asset_server: AssetServer = panic!();
/// let gltf_handle: Handle<Gltf> = asset_server.load_with_settings(
///     "my.gltf",
///     |s: &mut GltfLoaderSettings| {
///         s.load_cameras = false;
///     }
/// );
/// ```
#[derive(Serialize, Deserialize)]
pub struct GltfLoaderSettings {
    /// If empty, the gltf mesh nodes will be skipped.
    ///
    /// Otherwise, nodes will be loaded and retained in RAM/VRAM according to the active flags.
    pub load_meshes: RenderAssetUsages,
    /// If empty, the gltf materials will be skipped.
    ///
    /// Otherwise, materials will be loaded and retained in RAM/VRAM according to the active flags.
    pub load_materials: RenderAssetUsages,
    /// If true, the loader will spawn cameras for gltf camera nodes.
    pub load_cameras: bool,
    /// If true, the loader will spawn lights for gltf light nodes.
    pub load_lights: bool,
    /// If true, the loader will include the root of the gltf root node.
    pub include_source: bool,
}

impl Default for GltfLoaderSettings {
    fn default() -> Self {
        Self {
            load_meshes: RenderAssetUsages::default(),
            load_materials: RenderAssetUsages::default(),
            load_cameras: true,
            load_lights: true,
            include_source: false,
        }
    }
}

impl AssetLoader for GltfLoader {
    type Asset = Gltf;
    type Settings = GltfLoaderSettings;
    type Error = GltfError;
    async fn load(
        &self,
        reader: &mut dyn Reader,
        settings: &GltfLoaderSettings,
        load_context: &mut LoadContext<'_>,
    ) -> Result<Gltf, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        load_gltf(self, &bytes, load_context, settings).await
    }

    fn extensions(&self) -> &[&str] {
        &["gltf", "glb"]
    }
}

/// Loads an entire glTF file.
async fn load_gltf<'a, 'b, 'c>(
    loader: &GltfLoader,
    bytes: &'a [u8],
    load_context: &'b mut LoadContext<'c>,
    settings: &'b GltfLoaderSettings,
) -> Result<Gltf, GltfError> {
    let gltf = gltf::Gltf::from_slice(bytes)?;
    let file_name = load_context
        .asset_path()
        .path()
        .to_str()
        .ok_or(GltfError::Gltf(gltf::Error::Io(Error::new(
            std::io::ErrorKind::InvalidInput,
            "Gltf file name invalid",
        ))))?
        .to_string();
    let buffer_data = load_buffers(&gltf, load_context).await?;

    let mut linear_textures = <HashSet<_>>::default();

    for material in gltf.materials() {
        if let Some(texture) = material.normal_texture() {
            linear_textures.insert(texture.texture().index());
        }
        if let Some(texture) = material.occlusion_texture() {
            linear_textures.insert(texture.texture().index());
        }
        if let Some(texture) = material
            .pbr_metallic_roughness()
            .metallic_roughness_texture()
        {
            linear_textures.insert(texture.texture().index());
        }
        if let Some(texture_index) = material_extension_texture_index(
            &material,
            "KHR_materials_anisotropy",
            "anisotropyTexture",
        ) {
            linear_textures.insert(texture_index);
        }

        // None of the clearcoat maps should be loaded as sRGB.
        #[cfg(feature = "pbr_multi_layer_material_textures")]
        for texture_field_name in [
            "clearcoatTexture",
            "clearcoatRoughnessTexture",
            "clearcoatNormalTexture",
        ] {
            if let Some(texture_index) = material_extension_texture_index(
                &material,
                "KHR_materials_clearcoat",
                texture_field_name,
            ) {
                linear_textures.insert(texture_index);
            }
        }
    }

    #[cfg(feature = "bevy_animation")]
    let paths = {
        let mut paths = HashMap::<usize, (usize, Vec<Name>)>::default();
        for scene in gltf.scenes() {
            for node in scene.nodes() {
                let root_index = node.index();
                paths_recur(node, &[], &mut paths, root_index, &mut HashSet::default());
            }
        }
        paths
    };

    #[cfg(feature = "bevy_animation")]
    let (animations, named_animations, animation_roots) = {
        use bevy_animation::{animated_field, animation_curves::*, gltf_curves::*, VariableCurve};
        use bevy_math::{
            curve::{ConstantCurve, Interval, UnevenSampleAutoCurve},
            Quat, Vec4,
        };
        use gltf::animation::util::ReadOutputs;
        let mut animations = vec![];
        let mut named_animations = <HashMap<_, _>>::default();
        let mut animation_roots = <HashSet<_>>::default();
        for animation in gltf.animations() {
            let mut animation_clip = AnimationClip::default();
            for channel in animation.channels() {
                let node = channel.target().node();
                let interpolation = channel.sampler().interpolation();
                let reader = channel.reader(|buffer| Some(&buffer_data[buffer.index()]));
                let keyframe_timestamps: Vec<f32> = if let Some(inputs) = reader.read_inputs() {
                    match inputs {
                        Iter::Standard(times) => times.collect(),
                        Iter::Sparse(_) => {
                            warn!("Sparse accessor not supported for animation sampler input");
                            continue;
                        }
                    }
                } else {
                    warn!("Animations without a sampler input are not supported");
                    return Err(GltfError::MissingAnimationSampler(animation.index()));
                };

                if keyframe_timestamps.is_empty() {
                    warn!("Tried to load animation with no keyframe timestamps");
                    continue;
                }

                let maybe_curve: Option<VariableCurve> = if let Some(outputs) =
                    reader.read_outputs()
                {
                    match outputs {
                        ReadOutputs::Translations(tr) => {
                            let translation_property = animated_field!(Transform::translation);
                            let translations: Vec<Vec3> = tr.map(Vec3::from).collect();
                            if keyframe_timestamps.len() == 1 {
                                Some(VariableCurve::new(AnimatableCurve::new(
                                    translation_property,
                                    ConstantCurve::new(Interval::EVERYWHERE, translations[0]),
                                )))
                            } else {
                                match interpolation {
                                    gltf::animation::Interpolation::Linear => {
                                        UnevenSampleAutoCurve::new(
                                            keyframe_timestamps.into_iter().zip(translations),
                                        )
                                        .ok()
                                        .map(|curve| {
                                            VariableCurve::new(AnimatableCurve::new(
                                                translation_property,
                                                curve,
                                            ))
                                        })
                                    }
                                    gltf::animation::Interpolation::Step => {
                                        SteppedKeyframeCurve::new(
                                            keyframe_timestamps.into_iter().zip(translations),
                                        )
                                        .ok()
                                        .map(|curve| {
                                            VariableCurve::new(AnimatableCurve::new(
                                                translation_property,
                                                curve,
                                            ))
                                        })
                                    }
                                    gltf::animation::Interpolation::CubicSpline => {
                                        CubicKeyframeCurve::new(keyframe_timestamps, translations)
                                            .ok()
                                            .map(|curve| {
                                                VariableCurve::new(AnimatableCurve::new(
                                                    translation_property,
                                                    curve,
                                                ))
                                            })
                                    }
                                }
                            }
                        }
                        ReadOutputs::Rotations(rots) => {
                            let rotation_property = animated_field!(Transform::rotation);
                            let rotations: Vec<Quat> =
                                rots.into_f32().map(Quat::from_array).collect();
                            if keyframe_timestamps.len() == 1 {
                                Some(VariableCurve::new(AnimatableCurve::new(
                                    rotation_property,
                                    ConstantCurve::new(Interval::EVERYWHERE, rotations[0]),
                                )))
                            } else {
                                match interpolation {
                                    gltf::animation::Interpolation::Linear => {
                                        UnevenSampleAutoCurve::new(
                                            keyframe_timestamps.into_iter().zip(rotations),
                                        )
                                        .ok()
                                        .map(|curve| {
                                            VariableCurve::new(AnimatableCurve::new(
                                                rotation_property,
                                                curve,
                                            ))
                                        })
                                    }
                                    gltf::animation::Interpolation::Step => {
                                        SteppedKeyframeCurve::new(
                                            keyframe_timestamps.into_iter().zip(rotations),
                                        )
                                        .ok()
                                        .map(|curve| {
                                            VariableCurve::new(AnimatableCurve::new(
                                                rotation_property,
                                                curve,
                                            ))
                                        })
                                    }
                                    gltf::animation::Interpolation::CubicSpline => {
                                        CubicRotationCurve::new(
                                            keyframe_timestamps,
                                            rotations.into_iter().map(Vec4::from),
                                        )
                                        .ok()
                                        .map(|curve| {
                                            VariableCurve::new(AnimatableCurve::new(
                                                rotation_property,
                                                curve,
                                            ))
                                        })
                                    }
                                }
                            }
                        }
                        ReadOutputs::Scales(scale) => {
                            let scale_property = animated_field!(Transform::scale);
                            let scales: Vec<Vec3> = scale.map(Vec3::from).collect();
                            if keyframe_timestamps.len() == 1 {
                                Some(VariableCurve::new(AnimatableCurve::new(
                                    scale_property,
                                    ConstantCurve::new(Interval::EVERYWHERE, scales[0]),
                                )))
                            } else {
                                match interpolation {
                                    gltf::animation::Interpolation::Linear => {
                                        UnevenSampleAutoCurve::new(
                                            keyframe_timestamps.into_iter().zip(scales),
                                        )
                                        .ok()
                                        .map(|curve| {
                                            VariableCurve::new(AnimatableCurve::new(
                                                scale_property,
                                                curve,
                                            ))
                                        })
                                    }
                                    gltf::animation::Interpolation::Step => {
                                        SteppedKeyframeCurve::new(
                                            keyframe_timestamps.into_iter().zip(scales),
                                        )
                                        .ok()
                                        .map(|curve| {
                                            VariableCurve::new(AnimatableCurve::new(
                                                scale_property,
                                                curve,
                                            ))
                                        })
                                    }
                                    gltf::animation::Interpolation::CubicSpline => {
                                        CubicKeyframeCurve::new(keyframe_timestamps, scales)
                                            .ok()
                                            .map(|curve| {
                                                VariableCurve::new(AnimatableCurve::new(
                                                    scale_property,
                                                    curve,
                                                ))
                                            })
                                    }
                                }
                            }
                        }
                        ReadOutputs::MorphTargetWeights(weights) => {
                            let weights: Vec<f32> = weights.into_f32().collect();
                            if keyframe_timestamps.len() == 1 {
                                #[expect(
                                    clippy::unnecessary_map_on_constructor,
                                    reason = "While the mapping is unnecessary, it is much more readable at this level of indentation. Additionally, mapping makes it more consistent with the other branches."
                                )]
                                Some(ConstantCurve::new(Interval::EVERYWHERE, weights))
                                    .map(WeightsCurve)
                                    .map(VariableCurve::new)
                            } else {
                                match interpolation {
                                    gltf::animation::Interpolation::Linear => {
                                        WideLinearKeyframeCurve::new(keyframe_timestamps, weights)
                                            .ok()
                                            .map(WeightsCurve)
                                            .map(VariableCurve::new)
                                    }
                                    gltf::animation::Interpolation::Step => {
                                        WideSteppedKeyframeCurve::new(keyframe_timestamps, weights)
                                            .ok()
                                            .map(WeightsCurve)
                                            .map(VariableCurve::new)
                                    }
                                    gltf::animation::Interpolation::CubicSpline => {
                                        WideCubicKeyframeCurve::new(keyframe_timestamps, weights)
                                            .ok()
                                            .map(WeightsCurve)
                                            .map(VariableCurve::new)
                                    }
                                }
                            }
                        }
                    }
                } else {
                    warn!("Animations without a sampler output are not supported");
                    return Err(GltfError::MissingAnimationSampler(animation.index()));
                };

                let Some(curve) = maybe_curve else {
                    warn!(
                        "Invalid keyframe data for node {}; curve could not be constructed",
                        node.index()
                    );
                    continue;
                };

                if let Some((root_index, path)) = paths.get(&node.index()) {
                    animation_roots.insert(*root_index);
                    animation_clip.add_variable_curve_to_target(
                        AnimationTargetId::from_names(path.iter()),
                        curve,
                    );
                } else {
                    warn!(
                        "Animation ignored for node {}: part of its hierarchy is missing a name",
                        node.index()
                    );
                }
            }
            let handle = load_context.add_labeled_asset(
                GltfAssetLabel::Animation(animation.index()).to_string(),
                animation_clip,
            );
            if let Some(name) = animation.name() {
                named_animations.insert(name.into(), handle.clone());
            }
            animations.push(handle);
        }
        (animations, named_animations, animation_roots)
    };

    // TODO: use the threaded impl on wasm once wasm thread pool doesn't deadlock on it
    // See https://github.com/bevyengine/bevy/issues/1924 for more details
    // The taskpool use is also avoided when there is only one texture for performance reasons and
    // to avoid https://github.com/bevyengine/bevy/pull/2725
    // PERF: could this be a Vec instead? Are gltf texture indices dense?
    fn process_loaded_texture(
        load_context: &mut LoadContext,
        handles: &mut Vec<Handle<Image>>,
        texture: ImageOrPath,
    ) {
        let handle = match texture {
            ImageOrPath::Image { label, image } => {
                load_context.add_labeled_asset(label.to_string(), image)
            }
            ImageOrPath::Path {
                path,
                is_srgb,
                sampler_descriptor,
            } => load_context
                .loader()
                .with_settings(move |settings: &mut ImageLoaderSettings| {
                    settings.is_srgb = is_srgb;
                    settings.sampler = ImageSampler::Descriptor(sampler_descriptor.clone());
                })
                .load(path),
        };
        handles.push(handle);
    }

    // We collect handles to ensure loaded images from paths are not unloaded before they are used elsewhere
    // in the loader. This prevents "reloads", but it also prevents dropping the is_srgb context on reload.
    //
    // In theory we could store a mapping between texture.index() and handle to use
    // later in the loader when looking up handles for materials. However this would mean
    // that the material's load context would no longer track those images as dependencies.
    let mut _texture_handles = Vec::new();
    if gltf.textures().len() == 1 || cfg!(target_arch = "wasm32") {
        for texture in gltf.textures() {
            let parent_path = load_context.path().parent().unwrap();
            let image = load_image(
                texture,
                &buffer_data,
                &linear_textures,
                parent_path,
                loader.supported_compressed_formats,
                settings.load_materials,
            )
            .await?;
            process_loaded_texture(load_context, &mut _texture_handles, image);
        }
    } else {
        #[cfg(not(target_arch = "wasm32"))]
        IoTaskPool::get()
            .scope(|scope| {
                gltf.textures().for_each(|gltf_texture| {
                    let parent_path = load_context.path().parent().unwrap();
                    let linear_textures = &linear_textures;
                    let buffer_data = &buffer_data;
                    scope.spawn(async move {
                        load_image(
                            gltf_texture,
                            buffer_data,
                            linear_textures,
                            parent_path,
                            loader.supported_compressed_formats,
                            settings.load_materials,
                        )
                        .await
                    });
                });
            })
            .into_iter()
            .for_each(|result| match result {
                Ok(image) => {
                    process_loaded_texture(load_context, &mut _texture_handles, image);
                }
                Err(err) => {
                    warn!("Error loading glTF texture: {}", err);
                }
            });
    }

    let mut materials = vec![];
    let mut named_materials = <HashMap<_, _>>::default();
    // Only include materials in the output if they're set to be retained in the MAIN_WORLD and/or RENDER_WORLD by the load_materials flag
    if !settings.load_materials.is_empty() {
        // NOTE: materials must be loaded after textures because image load() calls will happen before load_with_settings, preventing is_srgb from being set properly
        for material in gltf.materials() {
            let handle = load_material(&material, load_context, &gltf.document, false);
            if let Some(name) = material.name() {
                named_materials.insert(name.into(), handle.clone());
            }
            materials.push(handle);
        }
    }
    let mut meshes = vec![];
    let mut named_meshes = <HashMap<_, _>>::default();
    let mut meshes_on_skinned_nodes = <HashSet<_>>::default();
    let mut meshes_on_non_skinned_nodes = <HashSet<_>>::default();
    for gltf_node in gltf.nodes() {
        if gltf_node.skin().is_some() {
            if let Some(mesh) = gltf_node.mesh() {
                meshes_on_skinned_nodes.insert(mesh.index());
            }
        } else if let Some(mesh) = gltf_node.mesh() {
            meshes_on_non_skinned_nodes.insert(mesh.index());
        }
    }
    for gltf_mesh in gltf.meshes() {
        let mut primitives = vec![];
        for primitive in gltf_mesh.primitives() {
            let primitive_label = GltfAssetLabel::Primitive {
                mesh: gltf_mesh.index(),
                primitive: primitive.index(),
            };
            let primitive_topology = get_primitive_topology(primitive.mode())?;

            let mut mesh = Mesh::new(primitive_topology, settings.load_meshes);

            // Read vertex attributes
            for (semantic, accessor) in primitive.attributes() {
                if [Semantic::Joints(0), Semantic::Weights(0)].contains(&semantic) {
                    if !meshes_on_skinned_nodes.contains(&gltf_mesh.index()) {
                        warn!(
                        "Ignoring attribute {:?} for skinned mesh {} used on non skinned nodes (NODE_SKINNED_MESH_WITHOUT_SKIN)",
                        semantic,
                        primitive_label
                    );
                        continue;
                    } else if meshes_on_non_skinned_nodes.contains(&gltf_mesh.index()) {
                        error!("Skinned mesh {} used on both skinned and non skin nodes, this is likely to cause an error (NODE_SKINNED_MESH_WITHOUT_SKIN)", primitive_label);
                    }
                }
                match convert_attribute(
                    semantic,
                    accessor,
                    &buffer_data,
                    &loader.custom_vertex_attributes,
                ) {
                    Ok((attribute, values)) => mesh.insert_attribute(attribute, values),
                    Err(err) => warn!("{}", err),
                }
            }

            // Read vertex indices
            let reader = primitive.reader(|buffer| Some(buffer_data[buffer.index()].as_slice()));
            if let Some(indices) = reader.read_indices() {
                mesh.insert_indices(match indices {
                    ReadIndices::U8(is) => Indices::U16(is.map(|x| x as u16).collect()),
                    ReadIndices::U16(is) => Indices::U16(is.collect()),
                    ReadIndices::U32(is) => Indices::U32(is.collect()),
                });
            };

            {
                let morph_target_reader = reader.read_morph_targets();
                if morph_target_reader.len() != 0 {
                    let morph_targets_label = GltfAssetLabel::MorphTarget {
                        mesh: gltf_mesh.index(),
                        primitive: primitive.index(),
                    };
                    let morph_target_image = MorphTargetImage::new(
                        morph_target_reader.map(PrimitiveMorphAttributesIter),
                        mesh.count_vertices(),
                        RenderAssetUsages::default(),
                    )?;
                    let handle = load_context
                        .add_labeled_asset(morph_targets_label.to_string(), morph_target_image.0);

                    mesh.set_morph_targets(handle);
                    let extras = gltf_mesh.extras().as_ref();
                    if let Some(names) = extras.and_then(|extras| {
                        serde_json::from_str::<MorphTargetNames>(extras.get()).ok()
                    }) {
                        mesh.set_morph_target_names(names.target_names);
                    }
                }
            }

            if mesh.attribute(Mesh::ATTRIBUTE_NORMAL).is_none()
                && matches!(mesh.primitive_topology(), PrimitiveTopology::TriangleList)
            {
                tracing::debug!("Automatically calculating missing vertex normals for geometry.");
                let vertex_count_before = mesh.count_vertices();
                mesh.duplicate_vertices();
                mesh.compute_flat_normals();
                let vertex_count_after = mesh.count_vertices();
                if vertex_count_before != vertex_count_after {
                    tracing::debug!("Missing vertex normals in indexed geometry, computing them as flat. Vertex count increased from {} to {}", vertex_count_before, vertex_count_after);
                } else {
                    tracing::debug!(
                        "Missing vertex normals in indexed geometry, computing them as flat."
                    );
                }
            }

            if let Some(vertex_attribute) = reader
                .read_tangents()
                .map(|v| VertexAttributeValues::Float32x4(v.collect()))
            {
                mesh.insert_attribute(Mesh::ATTRIBUTE_TANGENT, vertex_attribute);
            } else if mesh.attribute(Mesh::ATTRIBUTE_NORMAL).is_some()
                && material_needs_tangents(&primitive.material())
            {
                tracing::debug!(
                    "Missing vertex tangents for {}, computing them using the mikktspace algorithm. Consider using a tool such as Blender to pre-compute the tangents.", file_name
                );

                let generate_tangents_span = info_span!("generate_tangents", name = file_name);

                generate_tangents_span.in_scope(|| {
                    if let Err(err) = mesh.generate_tangents() {
                        warn!(
                            "Failed to generate vertex tangents using the mikktspace algorithm: {}",
                            err
                        );
                    }
                });
            }

            let mesh_handle = load_context.add_labeled_asset(primitive_label.to_string(), mesh);
            primitives.push(super::GltfPrimitive::new(
                &gltf_mesh,
                &primitive,
                mesh_handle,
                primitive
                    .material()
                    .index()
                    .and_then(|i| materials.get(i).cloned()),
                get_gltf_extras(primitive.extras()),
                get_gltf_extras(primitive.material().extras()),
            ));
        }

        let mesh =
            super::GltfMesh::new(&gltf_mesh, primitives, get_gltf_extras(gltf_mesh.extras()));

        let handle = load_context.add_labeled_asset(mesh.asset_label().to_string(), mesh);
        if let Some(name) = gltf_mesh.name() {
            named_meshes.insert(name.into(), handle.clone());
        }
        meshes.push(handle);
    }

    let skinned_mesh_inverse_bindposes: Vec<_> = gltf
        .skins()
        .map(|gltf_skin| {
            let reader = gltf_skin.reader(|buffer| Some(&buffer_data[buffer.index()]));
            let local_to_bone_bind_matrices: Vec<Mat4> = reader
                .read_inverse_bind_matrices()
                .unwrap()
                .map(|mat| Mat4::from_cols_array_2d(&mat))
                .collect();

            load_context.add_labeled_asset(
                inverse_bind_matrices_label(&gltf_skin),
                SkinnedMeshInverseBindposes::from(local_to_bone_bind_matrices),
            )
        })
        .collect();

    let mut nodes = HashMap::<usize, Handle<GltfNode>>::default();
    let mut named_nodes = <HashMap<_, _>>::default();
    let mut skins = vec![];
    let mut named_skins = <HashMap<_, _>>::default();
    for node in GltfTreeIterator::try_new(&gltf)? {
        let skin = node.skin().map(|skin| {
            let joints = skin
                .joints()
                .map(|joint| nodes.get(&joint.index()).unwrap().clone())
                .collect();

            let gltf_skin = GltfSkin::new(
                &skin,
                joints,
                skinned_mesh_inverse_bindposes[skin.index()].clone(),
                get_gltf_extras(skin.extras()),
            );

            let handle = load_context.add_labeled_asset(skin_label(&skin), gltf_skin);

            skins.push(handle.clone());
            if let Some(name) = skin.name() {
                named_skins.insert(name.into(), handle.clone());
            }

            handle
        });

        let children = node
            .children()
            .map(|child| nodes.get(&child.index()).unwrap().clone())
            .collect();

        let mesh = node
            .mesh()
            .map(|mesh| mesh.index())
            .and_then(|i| meshes.get(i).cloned());

        let gltf_node = GltfNode::new(
            &node,
            children,
            mesh,
            node_transform(&node),
            skin,
            get_gltf_extras(node.extras()),
        );

        #[cfg(feature = "bevy_animation")]
        let gltf_node = gltf_node.with_animation_root(animation_roots.contains(&node.index()));

        let handle = load_context.add_labeled_asset(gltf_node.asset_label().to_string(), gltf_node);
        nodes.insert(node.index(), handle.clone());
        if let Some(name) = node.name() {
            named_nodes.insert(name.into(), handle);
        }
    }

    let mut nodes_to_sort = nodes.into_iter().collect::<Vec<_>>();
    nodes_to_sort.sort_by_key(|(i, _)| *i);
    let nodes = nodes_to_sort
        .into_iter()
        .map(|(_, resolved)| resolved)
        .collect();

    let mut scenes = vec![];
    let mut named_scenes = <HashMap<_, _>>::default();
    let mut active_camera_found = false;
    for scene in gltf.scenes() {
        let mut err = None;
        let mut world = World::default();
        let mut node_index_to_entity_map = <HashMap<_, _>>::default();
        let mut entity_to_skin_index_map = EntityHashMap::default();
        let mut scene_load_context = load_context.begin_labeled_asset();

        let world_root_id = world
            .spawn((Transform::default(), Visibility::default()))
            .with_children(|parent| {
                for node in scene.nodes() {
                    let result = load_node(
                        &node,
                        parent,
                        load_context,
                        &mut scene_load_context,
                        settings,
                        &mut node_index_to_entity_map,
                        &mut entity_to_skin_index_map,
                        &mut active_camera_found,
                        &Transform::default(),
                        #[cfg(feature = "bevy_animation")]
                        &animation_roots,
                        #[cfg(feature = "bevy_animation")]
                        None,
                        &gltf.document,
                    );
                    if result.is_err() {
                        err = Some(result);
                        return;
                    }
                }
            })
            .id();

        if let Some(extras) = scene.extras().as_ref() {
            world.entity_mut(world_root_id).insert(GltfSceneExtras {
                value: extras.get().to_string(),
            });
        }

        if let Some(Err(err)) = err {
            return Err(err);
        }

        #[cfg(feature = "bevy_animation")]
        {
            // for each node root in a scene, check if it's the root of an animation
            // if it is, add the AnimationPlayer component
            for node in scene.nodes() {
                if animation_roots.contains(&node.index()) {
                    world
                        .entity_mut(*node_index_to_entity_map.get(&node.index()).unwrap())
                        .insert(AnimationPlayer::default());
                }
            }
        }

        for (&entity, &skin_index) in &entity_to_skin_index_map {
            let mut entity = world.entity_mut(entity);
            let skin = gltf.skins().nth(skin_index).unwrap();
            let joint_entities: Vec<_> = skin
                .joints()
                .map(|node| node_index_to_entity_map[&node.index()])
                .collect();

            entity.insert(SkinnedMesh {
                inverse_bindposes: skinned_mesh_inverse_bindposes[skin_index].clone(),
                joints: joint_entities,
            });
        }
        let loaded_scene = scene_load_context.finish(Scene::new(world));
        let scene_handle = load_context.add_loaded_labeled_asset(scene_label(&scene), loaded_scene);

        if let Some(name) = scene.name() {
            named_scenes.insert(name.into(), scene_handle.clone());
        }
        scenes.push(scene_handle);
    }

    Ok(Gltf {
        default_scene: gltf
            .default_scene()
            .and_then(|scene| scenes.get(scene.index()))
            .cloned(),
        scenes,
        named_scenes,
        meshes,
        named_meshes,
        skins,
        named_skins,
        materials,
        named_materials,
        nodes,
        named_nodes,
        #[cfg(feature = "bevy_animation")]
        animations,
        #[cfg(feature = "bevy_animation")]
        named_animations,
        source: if settings.include_source {
            Some(gltf)
        } else {
            None
        },
    })
}

fn get_gltf_extras(extras: &json::Extras) -> Option<GltfExtras> {
    extras.as_ref().map(|extras| GltfExtras {
        value: extras.get().to_string(),
    })
}

/// Calculate the transform of gLTF node.
///
/// This should be used instead of calling [`gltf::scene::Transform::matrix()`]
/// on [`Node::transform()`] directly because it uses optimized glam types and
/// if `libm` feature of `bevy_math` crate is enabled also handles cross
/// platform determinism properly.
fn node_transform(node: &Node) -> Transform {
    match node.transform() {
        gltf::scene::Transform::Matrix { matrix } => {
            Transform::from_matrix(Mat4::from_cols_array_2d(&matrix))
        }
        gltf::scene::Transform::Decomposed {
            translation,
            rotation,
            scale,
        } => Transform {
            translation: Vec3::from(translation),
            rotation: bevy_math::Quat::from_array(rotation),
            scale: Vec3::from(scale),
        },
    }
}

fn node_name(node: &Node) -> Name {
    let name = node
        .name()
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("GltfNode{}", node.index()));
    Name::new(name)
}

#[cfg(feature = "bevy_animation")]
fn paths_recur(
    node: Node,
    current_path: &[Name],
    paths: &mut HashMap<usize, (usize, Vec<Name>)>,
    root_index: usize,
    visited: &mut HashSet<usize>,
) {
    let mut path = current_path.to_owned();
    path.push(node_name(&node));
    visited.insert(node.index());
    for child in node.children() {
        if !visited.contains(&child.index()) {
            paths_recur(child, &path, paths, root_index, visited);
        }
    }
    paths.insert(node.index(), (root_index, path));
}

/// Loads a glTF texture as a bevy [`Image`] and returns it together with its label.
async fn load_image<'a, 'b>(
    gltf_texture: gltf::Texture<'a>,
    buffer_data: &[Vec<u8>],
    linear_textures: &HashSet<usize>,
    parent_path: &'b Path,
    supported_compressed_formats: CompressedImageFormats,
    render_asset_usages: RenderAssetUsages,
) -> Result<ImageOrPath, GltfError> {
    let is_srgb = !linear_textures.contains(&gltf_texture.index());
    let sampler_descriptor = texture_sampler(&gltf_texture);
    #[cfg(all(debug_assertions, feature = "dds"))]
    let name = gltf_texture
        .name()
        .map_or("Unknown GLTF Texture".to_string(), ToString::to_string);
    match gltf_texture.source().source() {
        Source::View { view, mime_type } => {
            let start = view.offset();
            let end = view.offset() + view.length();
            let buffer = &buffer_data[view.buffer().index()][start..end];
            let image = Image::from_buffer(
                #[cfg(all(debug_assertions, feature = "dds"))]
                name,
                buffer,
                ImageType::MimeType(mime_type),
                supported_compressed_formats,
                is_srgb,
                ImageSampler::Descriptor(sampler_descriptor),
                render_asset_usages,
            )?;
            Ok(ImageOrPath::Image {
                image,
                label: GltfAssetLabel::Texture(gltf_texture.index()),
            })
        }
        Source::Uri { uri, mime_type } => {
            let uri = percent_encoding::percent_decode_str(uri)
                .decode_utf8()
                .unwrap();
            let uri = uri.as_ref();
            if let Ok(data_uri) = DataUri::parse(uri) {
                let bytes = data_uri.decode()?;
                let image_type = ImageType::MimeType(data_uri.mime_type);
                Ok(ImageOrPath::Image {
                    image: Image::from_buffer(
                        #[cfg(all(debug_assertions, feature = "dds"))]
                        name,
                        &bytes,
                        mime_type.map(ImageType::MimeType).unwrap_or(image_type),
                        supported_compressed_formats,
                        is_srgb,
                        ImageSampler::Descriptor(sampler_descriptor),
                        render_asset_usages,
                    )?,
                    label: GltfAssetLabel::Texture(gltf_texture.index()),
                })
            } else {
                let image_path = parent_path.join(uri);
                Ok(ImageOrPath::Path {
                    path: image_path,
                    is_srgb,
                    sampler_descriptor,
                })
            }
        }
    }
}

/// Loads a glTF material as a bevy [`StandardMaterial`] and returns it.
fn load_material(
    material: &Material,
    load_context: &mut LoadContext,
    document: &Document,
    is_scale_inverted: bool,
) -> Handle<StandardMaterial> {
    let material_label = material_label(material, is_scale_inverted);
    load_context.labeled_asset_scope(material_label, |load_context| {
        let pbr = material.pbr_metallic_roughness();

        // TODO: handle missing label handle errors here?
        let color = pbr.base_color_factor();
        let base_color_channel = pbr
            .base_color_texture()
            .map(|info| get_uv_channel(material, "base color", info.tex_coord()))
            .unwrap_or_default();
        let base_color_texture = pbr
            .base_color_texture()
            .map(|info| texture_handle(load_context, &info.texture()));

        let uv_transform = pbr
            .base_color_texture()
            .and_then(|info| {
                info.texture_transform()
                    .map(convert_texture_transform_to_affine2)
            })
            .unwrap_or_default();

        let normal_map_channel = material
            .normal_texture()
            .map(|info| get_uv_channel(material, "normal map", info.tex_coord()))
            .unwrap_or_default();
        let normal_map_texture: Option<Handle<Image>> =
            material.normal_texture().map(|normal_texture| {
                // TODO: handle normal_texture.scale
                texture_handle(load_context, &normal_texture.texture())
            });

        let metallic_roughness_channel = pbr
            .metallic_roughness_texture()
            .map(|info| get_uv_channel(material, "metallic/roughness", info.tex_coord()))
            .unwrap_or_default();
        let metallic_roughness_texture = pbr.metallic_roughness_texture().map(|info| {
            warn_on_differing_texture_transforms(
                material,
                &info,
                uv_transform,
                "metallic/roughness",
            );
            texture_handle(load_context, &info.texture())
        });

        let occlusion_channel = material
            .occlusion_texture()
            .map(|info| get_uv_channel(material, "occlusion", info.tex_coord()))
            .unwrap_or_default();
        let occlusion_texture = material.occlusion_texture().map(|occlusion_texture| {
            // TODO: handle occlusion_texture.strength() (a scalar multiplier for occlusion strength)
            texture_handle(load_context, &occlusion_texture.texture())
        });

        let emissive = material.emissive_factor();
        let emissive_channel = material
            .emissive_texture()
            .map(|info| get_uv_channel(material, "emissive", info.tex_coord()))
            .unwrap_or_default();
        let emissive_texture = material.emissive_texture().map(|info| {
            // TODO: handle occlusion_texture.strength() (a scalar multiplier for occlusion strength)
            warn_on_differing_texture_transforms(material, &info, uv_transform, "emissive");
            texture_handle(load_context, &info.texture())
        });

        #[cfg(feature = "pbr_transmission_textures")]
        let (specular_transmission, specular_transmission_channel, specular_transmission_texture) =
            material
                .transmission()
                .map_or((0.0, UvChannel::Uv0, None), |transmission| {
                    let specular_transmission_channel = transmission
                        .transmission_texture()
                        .map(|info| {
                            get_uv_channel(material, "specular/transmission", info.tex_coord())
                        })
                        .unwrap_or_default();
                    let transmission_texture: Option<Handle<Image>> = transmission
                        .transmission_texture()
                        .map(|transmission_texture| {
                            texture_handle(load_context, &transmission_texture.texture())
                        });

                    (
                        transmission.transmission_factor(),
                        specular_transmission_channel,
                        transmission_texture,
                    )
                });

        #[cfg(not(feature = "pbr_transmission_textures"))]
        let specular_transmission = material
            .transmission()
            .map_or(0.0, |transmission| transmission.transmission_factor());

        #[cfg(feature = "pbr_transmission_textures")]
        let (
            thickness,
            thickness_channel,
            thickness_texture,
            attenuation_distance,
            attenuation_color,
        ) = material.volume().map_or(
            (0.0, UvChannel::Uv0, None, f32::INFINITY, [1.0, 1.0, 1.0]),
            |volume| {
                let thickness_channel = volume
                    .thickness_texture()
                    .map(|info| get_uv_channel(material, "thickness", info.tex_coord()))
                    .unwrap_or_default();
                let thickness_texture: Option<Handle<Image>> =
                    volume.thickness_texture().map(|thickness_texture| {
                        texture_handle(load_context, &thickness_texture.texture())
                    });

                (
                    volume.thickness_factor(),
                    thickness_channel,
                    thickness_texture,
                    volume.attenuation_distance(),
                    volume.attenuation_color(),
                )
            },
        );

        #[cfg(not(feature = "pbr_transmission_textures"))]
        let (thickness, attenuation_distance, attenuation_color) =
            material
                .volume()
                .map_or((0.0, f32::INFINITY, [1.0, 1.0, 1.0]), |volume| {
                    (
                        volume.thickness_factor(),
                        volume.attenuation_distance(),
                        volume.attenuation_color(),
                    )
                });

        let ior = material.ior().unwrap_or(1.5);

        // Parse the `KHR_materials_clearcoat` extension data if necessary.
        let clearcoat =
            ClearcoatExtension::parse(load_context, document, material).unwrap_or_default();

        // Parse the `KHR_materials_anisotropy` extension data if necessary.
        let anisotropy =
            AnisotropyExtension::parse(load_context, document, material).unwrap_or_default();

        // Parse the `KHR_materials_specular` extension data if necessary.
        let specular =
            SpecularExtension::parse(load_context, document, material).unwrap_or_default();

        // We need to operate in the Linear color space and be willing to exceed 1.0 in our channels
        let base_emissive = LinearRgba::rgb(emissive[0], emissive[1], emissive[2]);
        let emissive = base_emissive * material.emissive_strength().unwrap_or(1.0);

        StandardMaterial {
            base_color: Color::linear_rgba(color[0], color[1], color[2], color[3]),
            base_color_channel,
            base_color_texture,
            perceptual_roughness: pbr.roughness_factor(),
            metallic: pbr.metallic_factor(),
            metallic_roughness_channel,
            metallic_roughness_texture,
            normal_map_channel,
            normal_map_texture,
            double_sided: material.double_sided(),
            cull_mode: if material.double_sided() {
                None
            } else if is_scale_inverted {
                Some(Face::Front)
            } else {
                Some(Face::Back)
            },
            occlusion_channel,
            occlusion_texture,
            emissive,
            emissive_channel,
            emissive_texture,
            specular_transmission,
            #[cfg(feature = "pbr_transmission_textures")]
            specular_transmission_channel,
            #[cfg(feature = "pbr_transmission_textures")]
            specular_transmission_texture,
            thickness,
            #[cfg(feature = "pbr_transmission_textures")]
            thickness_channel,
            #[cfg(feature = "pbr_transmission_textures")]
            thickness_texture,
            ior,
            attenuation_distance,
            attenuation_color: Color::linear_rgb(
                attenuation_color[0],
                attenuation_color[1],
                attenuation_color[2],
            ),
            unlit: material.unlit(),
            alpha_mode: alpha_mode(material),
            uv_transform,
            clearcoat: clearcoat.clearcoat_factor.unwrap_or_default() as f32,
            clearcoat_perceptual_roughness: clearcoat.clearcoat_roughness_factor.unwrap_or_default()
                as f32,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_channel: clearcoat.clearcoat_channel,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_texture: clearcoat.clearcoat_texture,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_roughness_channel: clearcoat.clearcoat_roughness_channel,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_roughness_texture: clearcoat.clearcoat_roughness_texture,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_normal_channel: clearcoat.clearcoat_normal_channel,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_normal_texture: clearcoat.clearcoat_normal_texture,
            anisotropy_strength: anisotropy.anisotropy_strength.unwrap_or_default() as f32,
            anisotropy_rotation: anisotropy.anisotropy_rotation.unwrap_or_default() as f32,
            #[cfg(feature = "pbr_anisotropy_texture")]
            anisotropy_channel: anisotropy.anisotropy_channel,
            #[cfg(feature = "pbr_anisotropy_texture")]
            anisotropy_texture: anisotropy.anisotropy_texture,
            // From the `KHR_materials_specular` spec:
            // <https://github.com/KhronosGroup/glTF/tree/main/extensions/2.0/Khronos/KHR_materials_specular#materials-with-reflectance-parameter>
            reflectance: specular.specular_factor.unwrap_or(1.0) as f32 * 0.5,
            #[cfg(feature = "pbr_specular_textures")]
            specular_channel: specular.specular_channel,
            #[cfg(feature = "pbr_specular_textures")]
            specular_texture: specular.specular_texture,
            specular_tint: match specular.specular_color_factor {
                Some(color) => Color::linear_rgb(color[0] as f32, color[1] as f32, color[2] as f32),
                None => Color::WHITE,
            },
            #[cfg(feature = "pbr_specular_textures")]
            specular_tint_channel: specular.specular_color_channel,
            #[cfg(feature = "pbr_specular_textures")]
            specular_tint_texture: specular.specular_color_texture,
            ..Default::default()
        }
    })
}

fn get_uv_channel(material: &Material, texture_kind: &str, tex_coord: u32) -> UvChannel {
    match tex_coord {
        0 => UvChannel::Uv0,
        1 => UvChannel::Uv1,
        _ => {
            let material_name = material
                .name()
                .map(|n| format!("the material \"{n}\""))
                .unwrap_or_else(|| "an unnamed material".to_string());
            let material_index = material
                .index()
                .map(|i| format!("index {i}"))
                .unwrap_or_else(|| "default".to_string());
            warn!(
                "Only 2 UV Channels are supported, but {material_name} ({material_index}) \
                has the TEXCOORD attribute {} on texture kind {texture_kind}, which will fallback to 0.",
                tex_coord,
            );
            UvChannel::Uv0
        }
    }
}

fn convert_texture_transform_to_affine2(texture_transform: TextureTransform) -> Affine2 {
    Affine2::from_scale_angle_translation(
        texture_transform.scale().into(),
        -texture_transform.rotation(),
        texture_transform.offset().into(),
    )
}

fn warn_on_differing_texture_transforms(
    material: &Material,
    info: &Info,
    texture_transform: Affine2,
    texture_kind: &str,
) {
    let has_differing_texture_transform = info
        .texture_transform()
        .map(convert_texture_transform_to_affine2)
        .is_some_and(|t| t != texture_transform);
    if has_differing_texture_transform {
        let material_name = material
            .name()
            .map(|n| format!("the material \"{n}\""))
            .unwrap_or_else(|| "an unnamed material".to_string());
        let texture_name = info
            .texture()
            .name()
            .map(|n| format!("its {texture_kind} texture \"{n}\""))
            .unwrap_or_else(|| format!("its unnamed {texture_kind} texture"));
        let material_index = material
            .index()
            .map(|i| format!("index {i}"))
            .unwrap_or_else(|| "default".to_string());
        warn!(
            "Only texture transforms on base color textures are supported, but {material_name} ({material_index}) \
            has a texture transform on {texture_name} (index {}), which will be ignored.", info.texture().index()
        );
    }
}

/// Loads a glTF node.
#[expect(
    clippy::result_large_err,
    reason = "`GltfError` is only barely past the threshold for large errors."
)]
fn load_node(
    gltf_node: &Node,
    child_spawner: &mut ChildSpawner,
    root_load_context: &LoadContext,
    load_context: &mut LoadContext,
    settings: &GltfLoaderSettings,
    node_index_to_entity_map: &mut HashMap<usize, Entity>,
    entity_to_skin_index_map: &mut EntityHashMap<usize>,
    active_camera_found: &mut bool,
    parent_transform: &Transform,
    #[cfg(feature = "bevy_animation")] animation_roots: &HashSet<usize>,
    #[cfg(feature = "bevy_animation")] mut animation_context: Option<AnimationContext>,
    document: &Document,
) -> Result<(), GltfError> {
    let mut gltf_error = None;
    let transform = node_transform(gltf_node);
    let world_transform = *parent_transform * transform;
    // according to https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#instantiation,
    // if the determinant of the transform is negative we must invert the winding order of
    // triangles in meshes on the node.
    // instead we equivalently test if the global scale is inverted by checking if the number
    // of negative scale factors is odd. if so we will assign a copy of the material with face
    // culling inverted, rather than modifying the mesh data directly.
    let is_scale_inverted = world_transform.scale.is_negative_bitmask().count_ones() & 1 == 1;
    let mut node = child_spawner.spawn((transform, Visibility::default()));

    let name = node_name(gltf_node);
    node.insert(name.clone());

    #[cfg(feature = "bevy_animation")]
    if animation_context.is_none() && animation_roots.contains(&gltf_node.index()) {
        // This is an animation root. Make a new animation context.
        animation_context = Some(AnimationContext {
            root: node.id(),
            path: SmallVec::new(),
        });
    }

    #[cfg(feature = "bevy_animation")]
    if let Some(ref mut animation_context) = animation_context {
        animation_context.path.push(name);

        node.insert(AnimationTarget {
            id: AnimationTargetId::from_names(animation_context.path.iter()),
            player: animation_context.root,
        });
    }

    if let Some(extras) = gltf_node.extras() {
        node.insert(GltfExtras {
            value: extras.get().to_string(),
        });
    }

    // create camera node
    if settings.load_cameras {
        if let Some(camera) = gltf_node.camera() {
            let projection = match camera.projection() {
                gltf::camera::Projection::Orthographic(orthographic) => {
                    let xmag = orthographic.xmag();
                    let orthographic_projection = OrthographicProjection {
                        near: orthographic.znear(),
                        far: orthographic.zfar(),
                        scaling_mode: ScalingMode::FixedHorizontal {
                            viewport_width: xmag,
                        },
                        ..OrthographicProjection::default_3d()
                    };

                    Projection::Orthographic(orthographic_projection)
                }
                gltf::camera::Projection::Perspective(perspective) => {
                    let mut perspective_projection: PerspectiveProjection = PerspectiveProjection {
                        fov: perspective.yfov(),
                        near: perspective.znear(),
                        ..Default::default()
                    };
                    if let Some(zfar) = perspective.zfar() {
                        perspective_projection.far = zfar;
                    }
                    if let Some(aspect_ratio) = perspective.aspect_ratio() {
                        perspective_projection.aspect_ratio = aspect_ratio;
                    }
                    Projection::Perspective(perspective_projection)
                }
            };
            node.insert((
                Camera3d::default(),
                projection,
                transform,
                Camera {
                    is_active: !*active_camera_found,
                    ..Default::default()
                },
            ));

            *active_camera_found = true;
        }
    }

    // Map node index to entity
    node_index_to_entity_map.insert(gltf_node.index(), node.id());

    let mut morph_weights = None;

    node.with_children(|parent| {
        // Only include meshes in the output if they're set to be retained in the MAIN_WORLD and/or RENDER_WORLD by the load_meshes flag
        if !settings.load_meshes.is_empty() {
            if let Some(mesh) = gltf_node.mesh() {
                // append primitives
                for primitive in mesh.primitives() {
                    let material = primitive.material();
                    let material_label = material_label(&material, is_scale_inverted);

                    // This will make sure we load the default material now since it would not have been
                    // added when iterating over all the gltf materials (since the default material is
                    // not explicitly listed in the gltf).
                    // It also ensures an inverted scale copy is instantiated if required.
                    if !root_load_context.has_labeled_asset(&material_label)
                        && !load_context.has_labeled_asset(&material_label)
                    {
                        load_material(&material, load_context, document, is_scale_inverted);
                    }

                    let primitive_label = GltfAssetLabel::Primitive {
                        mesh: mesh.index(),
                        primitive: primitive.index(),
                    };
                    let bounds = primitive.bounding_box();

                    let mut mesh_entity = parent.spawn((
                        // TODO: handle missing label handle errors here?
                        Mesh3d(load_context.get_label_handle(primitive_label.to_string())),
                        MeshMaterial3d::<StandardMaterial>(
                            load_context.get_label_handle(&material_label),
                        ),
                    ));

                    let target_count = primitive.morph_targets().len();
                    if target_count != 0 {
                        let weights = match mesh.weights() {
                            Some(weights) => weights.to_vec(),
                            None => vec![0.0; target_count],
                        };

                        if morph_weights.is_none() {
                            morph_weights = Some(weights.clone());
                        }

                        // unwrap: the parent's call to `MeshMorphWeights::new`
                        // means this code doesn't run if it returns an `Err`.
                        // According to https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#morph-targets
                        // they should all have the same length.
                        // > All morph target accessors MUST have the same count as
                        // > the accessors of the original primitive.
                        mesh_entity.insert(MeshMorphWeights::new(weights).unwrap());
                    }
                    mesh_entity.insert(Aabb::from_min_max(
                        Vec3::from_slice(&bounds.min),
                        Vec3::from_slice(&bounds.max),
                    ));

                    if let Some(extras) = primitive.extras() {
                        mesh_entity.insert(GltfExtras {
                            value: extras.get().to_string(),
                        });
                    }

                    if let Some(extras) = mesh.extras() {
                        mesh_entity.insert(GltfMeshExtras {
                            value: extras.get().to_string(),
                        });
                    }

                    if let Some(extras) = material.extras() {
                        mesh_entity.insert(GltfMaterialExtras {
                            value: extras.get().to_string(),
                        });
                    }

                    if let Some(name) = material.name() {
                        mesh_entity.insert(GltfMaterialName(String::from(name)));
                    }

                    mesh_entity.insert(Name::new(primitive_name(&mesh, &primitive)));
                    // Mark for adding skinned mesh
                    if let Some(skin) = gltf_node.skin() {
                        entity_to_skin_index_map.insert(mesh_entity.id(), skin.index());
                    }
                }
            }
        }

        if settings.load_lights {
            if let Some(light) = gltf_node.light() {
                match light.kind() {
                    gltf::khr_lights_punctual::Kind::Directional => {
                        let mut entity = parent.spawn(DirectionalLight {
                            color: Color::srgb_from_array(light.color()),
                            // NOTE: KHR_punctual_lights defines the intensity units for directional
                            // lights in lux (lm/m^2) which is what we need.
                            illuminance: light.intensity(),
                            ..Default::default()
                        });
                        if let Some(name) = light.name() {
                            entity.insert(Name::new(name.to_string()));
                        }
                        if let Some(extras) = light.extras() {
                            entity.insert(GltfExtras {
                                value: extras.get().to_string(),
                            });
                        }
                    }
                    gltf::khr_lights_punctual::Kind::Point => {
                        let mut entity = parent.spawn(PointLight {
                            color: Color::srgb_from_array(light.color()),
                            // NOTE: KHR_punctual_lights defines the intensity units for point lights in
                            // candela (lm/sr) which is luminous intensity and we need luminous power.
                            // For a point light, luminous power = 4 * pi * luminous intensity
                            intensity: light.intensity() * core::f32::consts::PI * 4.0,
                            range: light.range().unwrap_or(20.0),
                            radius: 0.0,
                            ..Default::default()
                        });
                        if let Some(name) = light.name() {
                            entity.insert(Name::new(name.to_string()));
                        }
                        if let Some(extras) = light.extras() {
                            entity.insert(GltfExtras {
                                value: extras.get().to_string(),
                            });
                        }
                    }
                    gltf::khr_lights_punctual::Kind::Spot {
                        inner_cone_angle,
                        outer_cone_angle,
                    } => {
                        let mut entity = parent.spawn(SpotLight {
                            color: Color::srgb_from_array(light.color()),
                            // NOTE: KHR_punctual_lights defines the intensity units for spot lights in
                            // candela (lm/sr) which is luminous intensity and we need luminous power.
                            // For a spot light, we map luminous power = 4 * pi * luminous intensity
                            intensity: light.intensity() * core::f32::consts::PI * 4.0,
                            range: light.range().unwrap_or(20.0),
                            radius: light.range().unwrap_or(0.0),
                            inner_angle: inner_cone_angle,
                            outer_angle: outer_cone_angle,
                            ..Default::default()
                        });
                        if let Some(name) = light.name() {
                            entity.insert(Name::new(name.to_string()));
                        }
                        if let Some(extras) = light.extras() {
                            entity.insert(GltfExtras {
                                value: extras.get().to_string(),
                            });
                        }
                    }
                }
            }
        }

        // append other nodes
        for child in gltf_node.children() {
            if let Err(err) = load_node(
                &child,
                parent,
                root_load_context,
                load_context,
                settings,
                node_index_to_entity_map,
                entity_to_skin_index_map,
                active_camera_found,
                &world_transform,
                #[cfg(feature = "bevy_animation")]
                animation_roots,
                #[cfg(feature = "bevy_animation")]
                animation_context.clone(),
                document,
            ) {
                gltf_error = Some(err);
                return;
            }
        }
    });

    // Only include meshes in the output if they're set to be retained in the MAIN_WORLD and/or RENDER_WORLD by the load_meshes flag
    if !settings.load_meshes.is_empty() {
        if let (Some(mesh), Some(weights)) = (gltf_node.mesh(), morph_weights) {
            let primitive_label = mesh.primitives().next().map(|p| GltfAssetLabel::Primitive {
                mesh: mesh.index(),
                primitive: p.index(),
            });
            let first_mesh =
                primitive_label.map(|label| load_context.get_label_handle(label.to_string()));
            node.insert(MorphWeights::new(weights, first_mesh)?);
        }
    }

    if let Some(err) = gltf_error {
        Err(err)
    } else {
        Ok(())
    }
}

fn primitive_name(mesh: &gltf::Mesh, primitive: &Primitive) -> String {
    let mesh_name = mesh.name().unwrap_or("Mesh");
    if mesh.primitives().len() > 1 {
        format!("{}.{}", mesh_name, primitive.index())
    } else {
        mesh_name.to_string()
    }
}

/// Returns the label for the `material`.
fn material_label(material: &Material, is_scale_inverted: bool) -> String {
    if let Some(index) = material.index() {
        GltfAssetLabel::Material {
            index,
            is_scale_inverted,
        }
        .to_string()
    } else {
        GltfAssetLabel::DefaultMaterial.to_string()
    }
}

fn texture_handle(load_context: &mut LoadContext, texture: &gltf::Texture) -> Handle<Image> {
    match texture.source().source() {
        Source::View { .. } => {
            load_context.get_label_handle(GltfAssetLabel::Texture(texture.index()).to_string())
        }
        Source::Uri { uri, .. } => {
            let uri = percent_encoding::percent_decode_str(uri)
                .decode_utf8()
                .unwrap();
            let uri = uri.as_ref();
            if let Ok(_data_uri) = DataUri::parse(uri) {
                load_context.get_label_handle(GltfAssetLabel::Texture(texture.index()).to_string())
            } else {
                let parent = load_context.path().parent().unwrap();
                let image_path = parent.join(uri);
                load_context.load(image_path)
            }
        }
    }
}

/// Given a [`json::texture::Info`], returns the handle of the texture that this
/// refers to.
///
/// This is a low-level function only used when the `gltf` crate has no support
/// for an extension, forcing us to parse its texture references manually.
#[cfg(any(
    feature = "pbr_anisotropy_texture",
    feature = "pbr_multi_layer_material_textures",
    feature = "pbr_specular_textures"
))]
fn texture_handle_from_info(
    load_context: &mut LoadContext,
    document: &Document,
    texture_info: &json::texture::Info,
) -> Handle<Image> {
    let texture = document
        .textures()
        .nth(texture_info.index.value())
        .expect("Texture info references a nonexistent texture");
    texture_handle(load_context, &texture)
}

/// Returns the label for the `scene`.
fn scene_label(scene: &gltf::Scene) -> String {
    GltfAssetLabel::Scene(scene.index()).to_string()
}

/// Return the label for the `skin`.
fn skin_label(skin: &gltf::Skin) -> String {
    GltfAssetLabel::Skin(skin.index()).to_string()
}

/// Return the label for the `inverseBindMatrices` of the node.
fn inverse_bind_matrices_label(skin: &gltf::Skin) -> String {
    GltfAssetLabel::InverseBindMatrices(skin.index()).to_string()
}

/// Extracts the texture sampler data from the glTF texture.
fn texture_sampler(texture: &gltf::Texture) -> ImageSamplerDescriptor {
    let gltf_sampler = texture.sampler();

    ImageSamplerDescriptor {
        address_mode_u: texture_address_mode(&gltf_sampler.wrap_s()),
        address_mode_v: texture_address_mode(&gltf_sampler.wrap_t()),

        mag_filter: gltf_sampler
            .mag_filter()
            .map(|mf| match mf {
                MagFilter::Nearest => ImageFilterMode::Nearest,
                MagFilter::Linear => ImageFilterMode::Linear,
            })
            .unwrap_or(ImageSamplerDescriptor::default().mag_filter),

        min_filter: gltf_sampler
            .min_filter()
            .map(|mf| match mf {
                MinFilter::Nearest
                | MinFilter::NearestMipmapNearest
                | MinFilter::NearestMipmapLinear => ImageFilterMode::Nearest,
                MinFilter::Linear
                | MinFilter::LinearMipmapNearest
                | MinFilter::LinearMipmapLinear => ImageFilterMode::Linear,
            })
            .unwrap_or(ImageSamplerDescriptor::default().min_filter),

        mipmap_filter: gltf_sampler
            .min_filter()
            .map(|mf| match mf {
                MinFilter::Nearest
                | MinFilter::Linear
                | MinFilter::NearestMipmapNearest
                | MinFilter::LinearMipmapNearest => ImageFilterMode::Nearest,
                MinFilter::NearestMipmapLinear | MinFilter::LinearMipmapLinear => {
                    ImageFilterMode::Linear
                }
            })
            .unwrap_or(ImageSamplerDescriptor::default().mipmap_filter),

        ..Default::default()
    }
}

/// Maps the texture address mode from glTF to wgpu.
fn texture_address_mode(gltf_address_mode: &WrappingMode) -> ImageAddressMode {
    match gltf_address_mode {
        WrappingMode::ClampToEdge => ImageAddressMode::ClampToEdge,
        WrappingMode::Repeat => ImageAddressMode::Repeat,
        WrappingMode::MirroredRepeat => ImageAddressMode::MirrorRepeat,
    }
}

/// Maps the `primitive_topology` from glTF to `wgpu`.
#[expect(
    clippy::result_large_err,
    reason = "`GltfError` is only barely past the threshold for large errors."
)]
fn get_primitive_topology(mode: Mode) -> Result<PrimitiveTopology, GltfError> {
    match mode {
        Mode::Points => Ok(PrimitiveTopology::PointList),
        Mode::Lines => Ok(PrimitiveTopology::LineList),
        Mode::LineStrip => Ok(PrimitiveTopology::LineStrip),
        Mode::Triangles => Ok(PrimitiveTopology::TriangleList),
        Mode::TriangleStrip => Ok(PrimitiveTopology::TriangleStrip),
        mode => Err(GltfError::UnsupportedPrimitive { mode }),
    }
}

fn alpha_mode(material: &Material) -> AlphaMode {
    match material.alpha_mode() {
        gltf::material::AlphaMode::Opaque => AlphaMode::Opaque,
        gltf::material::AlphaMode::Mask => AlphaMode::Mask(material.alpha_cutoff().unwrap_or(0.5)),
        gltf::material::AlphaMode::Blend => AlphaMode::Blend,
    }
}

/// Loads the raw glTF buffer data for a specific glTF file.
async fn load_buffers(
    gltf: &gltf::Gltf,
    load_context: &mut LoadContext<'_>,
) -> Result<Vec<Vec<u8>>, GltfError> {
    const VALID_MIME_TYPES: &[&str] = &["application/octet-stream", "application/gltf-buffer"];

    let mut buffer_data = Vec::new();
    for buffer in gltf.buffers() {
        match buffer.source() {
            gltf::buffer::Source::Uri(uri) => {
                let uri = percent_encoding::percent_decode_str(uri)
                    .decode_utf8()
                    .unwrap();
                let uri = uri.as_ref();
                let buffer_bytes = match DataUri::parse(uri) {
                    Ok(data_uri) if VALID_MIME_TYPES.contains(&data_uri.mime_type) => {
                        data_uri.decode()?
                    }
                    Ok(_) => return Err(GltfError::BufferFormatUnsupported),
                    Err(()) => {
                        // TODO: Remove this and add dep
                        let buffer_path = load_context.path().parent().unwrap().join(uri);
                        load_context.read_asset_bytes(buffer_path).await?
                    }
                };
                buffer_data.push(buffer_bytes);
            }
            gltf::buffer::Source::Bin => {
                if let Some(blob) = gltf.blob.as_deref() {
                    buffer_data.push(blob.into());
                } else {
                    return Err(GltfError::MissingBlob);
                }
            }
        }
    }

    Ok(buffer_data)
}

/// Iterator for a Gltf tree.
///
/// It resolves a Gltf tree and allows for a safe Gltf nodes iteration,
/// putting dependent nodes before dependencies.
struct GltfTreeIterator<'a> {
    nodes: Vec<Node<'a>>,
}

impl<'a> GltfTreeIterator<'a> {
    #[expect(
        clippy::result_large_err,
        reason = "`GltfError` is only barely past the threshold for large errors."
    )]
    fn try_new(gltf: &'a gltf::Gltf) -> Result<Self, GltfError> {
        let nodes = gltf.nodes().collect::<Vec<_>>();

        let mut empty_children = VecDeque::new();
        let mut parents = vec![None; nodes.len()];
        let mut unprocessed_nodes = nodes
            .into_iter()
            .enumerate()
            .map(|(i, node)| {
                let children = node
                    .children()
                    .map(|child| child.index())
                    .collect::<HashSet<_>>();
                for &child in &children {
                    let parent = parents.get_mut(child).unwrap();
                    *parent = Some(i);
                }
                if children.is_empty() {
                    empty_children.push_back(i);
                }
                (i, (node, children))
            })
            .collect::<HashMap<_, _>>();

        let mut nodes = Vec::new();
        let mut warned_about_max_joints = <HashSet<_>>::default();
        while let Some(index) = empty_children.pop_front() {
            if let Some(skin) = unprocessed_nodes.get(&index).unwrap().0.skin() {
                if skin.joints().len() > MAX_JOINTS && warned_about_max_joints.insert(skin.index())
                {
                    warn!(
                        "The glTF skin {} has {} joints, but the maximum supported is {}",
                        skin.name()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| skin.index().to_string()),
                        skin.joints().len(),
                        MAX_JOINTS
                    );
                }

                let skin_has_dependencies = skin
                    .joints()
                    .any(|joint| unprocessed_nodes.contains_key(&joint.index()));

                if skin_has_dependencies && unprocessed_nodes.len() != 1 {
                    empty_children.push_back(index);
                    continue;
                }
            }

            let (node, children) = unprocessed_nodes.remove(&index).unwrap();
            assert!(children.is_empty());
            nodes.push(node);

            if let Some(parent_index) = parents[index] {
                let (_, parent_children) = unprocessed_nodes.get_mut(&parent_index).unwrap();

                assert!(parent_children.remove(&index));
                if parent_children.is_empty() {
                    empty_children.push_back(parent_index);
                }
            }
        }

        if !unprocessed_nodes.is_empty() {
            return Err(GltfError::CircularChildren(format!(
                "{:?}",
                unprocessed_nodes
                    .iter()
                    .map(|(k, _v)| *k)
                    .collect::<Vec<_>>(),
            )));
        }

        nodes.reverse();
        Ok(Self {
            nodes: nodes.into_iter().collect(),
        })
    }
}

impl<'a> Iterator for GltfTreeIterator<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.nodes.pop()
    }
}

impl<'a> ExactSizeIterator for GltfTreeIterator<'a> {
    fn len(&self) -> usize {
        self.nodes.len()
    }
}

enum ImageOrPath {
    Image {
        image: Image,
        label: GltfAssetLabel,
    },
    Path {
        path: PathBuf,
        is_srgb: bool,
        sampler_descriptor: ImageSamplerDescriptor,
    },
}

struct DataUri<'a> {
    mime_type: &'a str,
    base64: bool,
    data: &'a str,
}

fn split_once(input: &str, delimiter: char) -> Option<(&str, &str)> {
    let mut iter = input.splitn(2, delimiter);
    Some((iter.next()?, iter.next()?))
}

impl<'a> DataUri<'a> {
    fn parse(uri: &'a str) -> Result<DataUri<'a>, ()> {
        let uri = uri.strip_prefix("data:").ok_or(())?;
        let (mime_type, data) = split_once(uri, ',').ok_or(())?;

        let (mime_type, base64) = match mime_type.strip_suffix(";base64") {
            Some(mime_type) => (mime_type, true),
            None => (mime_type, false),
        };

        Ok(DataUri {
            mime_type,
            base64,
            data,
        })
    }

    fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        if self.base64 {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, self.data)
        } else {
            Ok(self.data.as_bytes().to_owned())
        }
    }
}

pub(super) struct PrimitiveMorphAttributesIter<'s>(
    pub  (
        Option<Iter<'s, [f32; 3]>>,
        Option<Iter<'s, [f32; 3]>>,
        Option<Iter<'s, [f32; 3]>>,
    ),
);
impl<'s> Iterator for PrimitiveMorphAttributesIter<'s> {
    type Item = MorphAttributes;

    fn next(&mut self) -> Option<Self::Item> {
        let position = self.0 .0.as_mut().and_then(Iterator::next);
        let normal = self.0 .1.as_mut().and_then(Iterator::next);
        let tangent = self.0 .2.as_mut().and_then(Iterator::next);
        if position.is_none() && normal.is_none() && tangent.is_none() {
            return None;
        }

        Some(MorphAttributes {
            position: position.map(Into::into).unwrap_or(Vec3::ZERO),
            normal: normal.map(Into::into).unwrap_or(Vec3::ZERO),
            tangent: tangent.map(Into::into).unwrap_or(Vec3::ZERO),
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MorphTargetNames {
    pub target_names: Vec<String>,
}

// A helper structure for `load_node` that contains information about the
// nearest ancestor animation root.
#[cfg(feature = "bevy_animation")]
#[derive(Clone)]
struct AnimationContext {
    // The nearest ancestor animation root.
    root: Entity,
    // The path to the animation root. This is used for constructing the
    // animation target UUIDs.
    path: SmallVec<[Name; 8]>,
}

/// Parsed data from the `KHR_materials_clearcoat` extension.
///
/// See the specification:
/// <https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_clearcoat/README.md>
#[derive(Default)]
struct ClearcoatExtension {
    clearcoat_factor: Option<f64>,
    #[cfg(feature = "pbr_multi_layer_material_textures")]
    clearcoat_channel: UvChannel,
    #[cfg(feature = "pbr_multi_layer_material_textures")]
    clearcoat_texture: Option<Handle<Image>>,
    clearcoat_roughness_factor: Option<f64>,
    #[cfg(feature = "pbr_multi_layer_material_textures")]
    clearcoat_roughness_channel: UvChannel,
    #[cfg(feature = "pbr_multi_layer_material_textures")]
    clearcoat_roughness_texture: Option<Handle<Image>>,
    #[cfg(feature = "pbr_multi_layer_material_textures")]
    clearcoat_normal_channel: UvChannel,
    #[cfg(feature = "pbr_multi_layer_material_textures")]
    clearcoat_normal_texture: Option<Handle<Image>>,
}

impl ClearcoatExtension {
    #[expect(
        clippy::allow_attributes,
        reason = "`unused_variables` is not always linted"
    )]
    #[allow(
        unused_variables,
        reason = "Depending on what features are used to compile this crate, certain parameters may end up unused."
    )]
    fn parse(
        load_context: &mut LoadContext,
        document: &Document,
        material: &Material,
    ) -> Option<ClearcoatExtension> {
        let extension = material
            .extensions()?
            .get("KHR_materials_clearcoat")?
            .as_object()?;

        #[cfg(feature = "pbr_multi_layer_material_textures")]
        let (clearcoat_channel, clearcoat_texture) = parse_material_extension_texture(
            load_context,
            document,
            material,
            extension,
            "clearcoatTexture",
            "clearcoat",
        );

        #[cfg(feature = "pbr_multi_layer_material_textures")]
        let (clearcoat_roughness_channel, clearcoat_roughness_texture) =
            parse_material_extension_texture(
                load_context,
                document,
                material,
                extension,
                "clearcoatRoughnessTexture",
                "clearcoat roughness",
            );

        #[cfg(feature = "pbr_multi_layer_material_textures")]
        let (clearcoat_normal_channel, clearcoat_normal_texture) = parse_material_extension_texture(
            load_context,
            document,
            material,
            extension,
            "clearcoatNormalTexture",
            "clearcoat normal",
        );

        Some(ClearcoatExtension {
            clearcoat_factor: extension.get("clearcoatFactor").and_then(Value::as_f64),
            clearcoat_roughness_factor: extension
                .get("clearcoatRoughnessFactor")
                .and_then(Value::as_f64),
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_channel,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_texture,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_roughness_channel,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_roughness_texture,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_normal_channel,
            #[cfg(feature = "pbr_multi_layer_material_textures")]
            clearcoat_normal_texture,
        })
    }
}

/// Parsed data from the `KHR_materials_anisotropy` extension.
///
/// See the specification:
/// <https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_anisotropy/README.md>
#[derive(Default)]
struct AnisotropyExtension {
    anisotropy_strength: Option<f64>,
    anisotropy_rotation: Option<f64>,
    #[cfg(feature = "pbr_anisotropy_texture")]
    anisotropy_channel: UvChannel,
    #[cfg(feature = "pbr_anisotropy_texture")]
    anisotropy_texture: Option<Handle<Image>>,
}

impl AnisotropyExtension {
    #[expect(
        clippy::allow_attributes,
        reason = "`unused_variables` is not always linted"
    )]
    #[allow(
        unused_variables,
        reason = "Depending on what features are used to compile this crate, certain parameters may end up unused."
    )]
    fn parse(
        load_context: &mut LoadContext,
        document: &Document,
        material: &Material,
    ) -> Option<AnisotropyExtension> {
        let extension = material
            .extensions()?
            .get("KHR_materials_anisotropy")?
            .as_object()?;

        #[cfg(feature = "pbr_anisotropy_texture")]
        let (anisotropy_channel, anisotropy_texture) = extension
            .get("anisotropyTexture")
            .and_then(|value| value::from_value::<json::texture::Info>(value.clone()).ok())
            .map(|json_info| {
                (
                    get_uv_channel(material, "anisotropy", json_info.tex_coord),
                    texture_handle_from_info(load_context, document, &json_info),
                )
            })
            .unzip();

        Some(AnisotropyExtension {
            anisotropy_strength: extension.get("anisotropyStrength").and_then(Value::as_f64),
            anisotropy_rotation: extension.get("anisotropyRotation").and_then(Value::as_f64),
            #[cfg(feature = "pbr_anisotropy_texture")]
            anisotropy_channel: anisotropy_channel.unwrap_or_default(),
            #[cfg(feature = "pbr_anisotropy_texture")]
            anisotropy_texture,
        })
    }
}

/// Parsed data from the `KHR_materials_specular` extension.
///
/// We currently don't parse `specularFactor` and `specularTexture`, since
/// they're incompatible with Filament.
///
/// Note that the map is a *specular map*, not a *reflectance map*. In Bevy and
/// Filament terms, the reflectance values in the specular map range from [0.0,
/// 0.5], rather than [0.0, 1.0]. This is an unfortunate
/// `KHR_materials_specular` specification requirement that stems from the fact
/// that glTF is specified in terms of a specular strength model, not the
/// reflectance model that Filament and Bevy use. A workaround, which is noted
/// in the [`StandardMaterial`] documentation, is to set the reflectance value
/// to 2.0, which spreads the specular map range from [0.0, 1.0] as normal.
///
/// See the specification:
/// <https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_specular/README.md>
#[derive(Default)]
struct SpecularExtension {
    specular_factor: Option<f64>,
    #[cfg(feature = "pbr_specular_textures")]
    specular_channel: UvChannel,
    #[cfg(feature = "pbr_specular_textures")]
    specular_texture: Option<Handle<Image>>,
    specular_color_factor: Option<[f64; 3]>,
    #[cfg(feature = "pbr_specular_textures")]
    specular_color_channel: UvChannel,
    #[cfg(feature = "pbr_specular_textures")]
    specular_color_texture: Option<Handle<Image>>,
}

impl SpecularExtension {
    fn parse(
        _load_context: &mut LoadContext,
        _document: &Document,
        material: &Material,
    ) -> Option<Self> {
        let extension = material
            .extensions()?
            .get("KHR_materials_specular")?
            .as_object()?;

        #[cfg(feature = "pbr_specular_textures")]
        let (_specular_channel, _specular_texture) = parse_material_extension_texture(
            _load_context,
            _document,
            material,
            extension,
            "specularTexture",
            "specular",
        );

        #[cfg(feature = "pbr_specular_textures")]
        let (_specular_color_channel, _specular_color_texture) = parse_material_extension_texture(
            _load_context,
            _document,
            material,
            extension,
            "specularColorTexture",
            "specular color",
        );

        Some(SpecularExtension {
            specular_factor: extension.get("specularFactor").and_then(Value::as_f64),
            #[cfg(feature = "pbr_specular_textures")]
            specular_channel: _specular_channel,
            #[cfg(feature = "pbr_specular_textures")]
            specular_texture: _specular_texture,
            specular_color_factor: extension
                .get("specularColorFactor")
                .and_then(Value::as_array)
                .and_then(|json_array| {
                    if json_array.len() < 3 {
                        None
                    } else {
                        Some([
                            json_array[0].as_f64()?,
                            json_array[1].as_f64()?,
                            json_array[2].as_f64()?,
                        ])
                    }
                }),
            #[cfg(feature = "pbr_specular_textures")]
            specular_color_channel: _specular_color_channel,
            #[cfg(feature = "pbr_specular_textures")]
            specular_color_texture: _specular_color_texture,
        })
    }
}

/// Parses a texture that's part of a material extension block and returns its
/// UV channel and image reference.
#[cfg(any(
    feature = "pbr_specular_textures",
    feature = "pbr_multi_layer_material_textures"
))]
fn parse_material_extension_texture(
    load_context: &mut LoadContext,
    document: &Document,
    material: &Material,
    extension: &Map<String, Value>,
    texture_name: &str,
    texture_kind: &str,
) -> (UvChannel, Option<Handle<Image>>) {
    match extension
        .get(texture_name)
        .and_then(|value| value::from_value::<json::texture::Info>(value.clone()).ok())
    {
        Some(json_info) => (
            get_uv_channel(material, texture_kind, json_info.tex_coord),
            Some(texture_handle_from_info(load_context, document, &json_info)),
        ),
        None => (UvChannel::default(), None),
    }
}

/// Returns the index (within the `textures` array) of the texture with the
/// given field name in the data for the material extension with the given name,
/// if there is one.
fn material_extension_texture_index(
    material: &Material,
    extension_name: &str,
    texture_field_name: &str,
) -> Option<usize> {
    Some(
        value::from_value::<json::texture::Info>(
            material
                .extensions()?
                .get(extension_name)?
                .as_object()?
                .get(texture_field_name)?
                .clone(),
        )
        .ok()?
        .index
        .value(),
    )
}

/// Returns true if the material needs mesh tangents in order to be successfully
/// rendered.
///
/// We generate them if this function returns true.
fn material_needs_tangents(material: &Material) -> bool {
    if material.normal_texture().is_some() {
        return true;
    }

    #[cfg(feature = "pbr_multi_layer_material_textures")]
    if material_extension_texture_index(
        material,
        "KHR_materials_clearcoat",
        "clearcoatNormalTexture",
    )
    .is_some()
    {
        return true;
    }

    false
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use crate::{Gltf, GltfAssetLabel, GltfNode, GltfSkin};
    use bevy_app::{App, TaskPoolPlugin};
    use bevy_asset::{
        io::{
            memory::{Dir, MemoryAssetReader},
            AssetSource, AssetSourceId,
        },
        AssetApp, AssetPlugin, AssetServer, Assets, Handle, LoadState,
    };
    use bevy_ecs::{resource::Resource, world::World};
    use bevy_log::LogPlugin;
    use bevy_render::mesh::{skinning::SkinnedMeshInverseBindposes, MeshPlugin};
    use bevy_scene::ScenePlugin;

    fn test_app(dir: Dir) -> App {
        let mut app = App::new();
        let reader = MemoryAssetReader { root: dir };
        app.register_asset_source(
            AssetSourceId::Default,
            AssetSource::build().with_reader(move || Box::new(reader.clone())),
        )
        .add_plugins((
            LogPlugin::default(),
            TaskPoolPlugin::default(),
            AssetPlugin::default(),
            ScenePlugin,
            MeshPlugin,
            crate::GltfPlugin::default(),
        ));

        app.finish();
        app.cleanup();

        app
    }

    const LARGE_ITERATION_COUNT: usize = 10000;

    fn run_app_until(app: &mut App, mut predicate: impl FnMut(&mut World) -> Option<()>) {
        for _ in 0..LARGE_ITERATION_COUNT {
            app.update();
            if predicate(app.world_mut()).is_some() {
                return;
            }
        }

        panic!("Ran out of loops to return `Some` from `predicate`");
    }

    fn load_gltf_into_app(gltf_path: &str, gltf: &str) -> App {
        #[expect(
            dead_code,
            reason = "This struct is used to keep the handle alive. As such, we have no need to handle the handle directly."
        )]
        #[derive(Resource)]
        struct GltfHandle(Handle<Gltf>);

        let dir = Dir::default();
        dir.insert_asset_text(Path::new(gltf_path), gltf);
        let mut app = test_app(dir);
        app.update();
        let asset_server = app.world().resource::<AssetServer>().clone();
        let handle: Handle<Gltf> = asset_server.load(gltf_path.to_string());
        let handle_id = handle.id();
        app.insert_resource(GltfHandle(handle));
        app.update();
        run_app_until(&mut app, |_world| {
            let load_state = asset_server.get_load_state(handle_id).unwrap();
            match load_state {
                LoadState::Loaded => Some(()),
                LoadState::Failed(err) => panic!("{err}"),
                _ => None,
            }
        });
        app
    }

    #[test]
    fn single_node() {
        let gltf_path = "test.gltf";
        let app = load_gltf_into_app(
            gltf_path,
            r#"
{
    "asset": {
        "version": "2.0"
    },
    "nodes": [
        {
            "name": "TestSingleNode"
        }
    ],
    "scene": 0,
    "scenes": [{ "nodes": [0] }]
}
"#,
        );
        let asset_server = app.world().resource::<AssetServer>();
        let handle = asset_server.load(gltf_path);
        let gltf_root_assets = app.world().resource::<Assets<Gltf>>();
        let gltf_node_assets = app.world().resource::<Assets<GltfNode>>();
        let gltf_root = gltf_root_assets.get(&handle).unwrap();
        assert!(gltf_root.nodes.len() == 1, "Single node");
        assert!(
            gltf_root.named_nodes.contains_key("TestSingleNode"),
            "Named node is in named nodes"
        );
        let gltf_node = gltf_node_assets
            .get(gltf_root.named_nodes.get("TestSingleNode").unwrap())
            .unwrap();
        assert_eq!(gltf_node.name, "TestSingleNode", "Correct name");
        assert_eq!(gltf_node.index, 0, "Correct index");
        assert_eq!(gltf_node.children.len(), 0, "No children");
        assert_eq!(gltf_node.asset_label(), GltfAssetLabel::Node(0));
    }

    #[test]
    fn node_hierarchy_no_hierarchy() {
        let gltf_path = "test.gltf";
        let app = load_gltf_into_app(
            gltf_path,
            r#"
{
    "asset": {
        "version": "2.0"
    },
    "nodes": [
        {
            "name": "l1"
        },
        {
            "name": "l2"
        }
    ],
    "scene": 0,
    "scenes": [{ "nodes": [0] }]
}
"#,
        );
        let asset_server = app.world().resource::<AssetServer>();
        let handle = asset_server.load(gltf_path);
        let gltf_root_assets = app.world().resource::<Assets<Gltf>>();
        let gltf_node_assets = app.world().resource::<Assets<GltfNode>>();
        let gltf_root = gltf_root_assets.get(&handle).unwrap();
        let result = gltf_root
            .nodes
            .iter()
            .map(|h| gltf_node_assets.get(h).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "l1");
        assert_eq!(result[0].children.len(), 0);
        assert_eq!(result[1].name, "l2");
        assert_eq!(result[1].children.len(), 0);
    }

    #[test]
    fn node_hierarchy_simple_hierarchy() {
        let gltf_path = "test.gltf";
        let app = load_gltf_into_app(
            gltf_path,
            r#"
{
    "asset": {
        "version": "2.0"
    },
    "nodes": [
        {
            "name": "l1",
            "children": [1]
        },
        {
            "name": "l2"
        }
    ],
    "scene": 0,
    "scenes": [{ "nodes": [0] }]
}
"#,
        );
        let asset_server = app.world().resource::<AssetServer>();
        let handle = asset_server.load(gltf_path);
        let gltf_root_assets = app.world().resource::<Assets<Gltf>>();
        let gltf_node_assets = app.world().resource::<Assets<GltfNode>>();
        let gltf_root = gltf_root_assets.get(&handle).unwrap();
        let result = gltf_root
            .nodes
            .iter()
            .map(|h| gltf_node_assets.get(h).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "l1");
        assert_eq!(result[0].children.len(), 1);
        assert_eq!(result[1].name, "l2");
        assert_eq!(result[1].children.len(), 0);
    }

    #[test]
    fn node_hierarchy_hierarchy() {
        let gltf_path = "test.gltf";
        let app = load_gltf_into_app(
            gltf_path,
            r#"
{
    "asset": {
        "version": "2.0"
    },
    "nodes": [
        {
            "name": "l1",
            "children": [1]
        },
        {
            "name": "l2",
            "children": [2]
        },
        {
            "name": "l3",
            "children": [3, 4, 5]
        },
        {
            "name": "l4",
            "children": [6]
        },
        {
            "name": "l5"
        },
        {
            "name": "l6"
        },
        {
            "name": "l7"
        }
    ],
    "scene": 0,
    "scenes": [{ "nodes": [0] }]
}
"#,
        );
        let asset_server = app.world().resource::<AssetServer>();
        let handle = asset_server.load(gltf_path);
        let gltf_root_assets = app.world().resource::<Assets<Gltf>>();
        let gltf_node_assets = app.world().resource::<Assets<GltfNode>>();
        let gltf_root = gltf_root_assets.get(&handle).unwrap();
        let result = gltf_root
            .nodes
            .iter()
            .map(|h| gltf_node_assets.get(h).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(result.len(), 7);
        assert_eq!(result[0].name, "l1");
        assert_eq!(result[0].children.len(), 1);
        assert_eq!(result[1].name, "l2");
        assert_eq!(result[1].children.len(), 1);
        assert_eq!(result[2].name, "l3");
        assert_eq!(result[2].children.len(), 3);
        assert_eq!(result[3].name, "l4");
        assert_eq!(result[3].children.len(), 1);
        assert_eq!(result[4].name, "l5");
        assert_eq!(result[4].children.len(), 0);
        assert_eq!(result[5].name, "l6");
        assert_eq!(result[5].children.len(), 0);
        assert_eq!(result[6].name, "l7");
        assert_eq!(result[6].children.len(), 0);
    }

    #[test]
    fn node_hierarchy_cyclic() {
        let gltf_path = "test.gltf";
        let gltf_str = r#"
{
    "asset": {
        "version": "2.0"
    },
    "nodes": [
        {
            "name": "l1",
            "children": [1]
        },
        {
            "name": "l2",
            "children": [0]
        }
    ],
    "scene": 0,
    "scenes": [{ "nodes": [0] }]
}
"#;

        let dir = Dir::default();
        dir.insert_asset_text(Path::new(gltf_path), gltf_str);
        let mut app = test_app(dir);
        app.update();
        let asset_server = app.world().resource::<AssetServer>().clone();
        let handle: Handle<Gltf> = asset_server.load(gltf_path);
        let handle_id = handle.id();
        app.update();
        run_app_until(&mut app, |_world| {
            let load_state = asset_server.get_load_state(handle_id).unwrap();
            if load_state.is_failed() {
                Some(())
            } else {
                None
            }
        });
        let load_state = asset_server.get_load_state(handle_id).unwrap();
        assert!(load_state.is_failed());
    }

    #[test]
    fn node_hierarchy_missing_node() {
        let gltf_path = "test.gltf";
        let gltf_str = r#"
{
    "asset": {
        "version": "2.0"
    },
    "nodes": [
        {
            "name": "l1",
            "children": [2]
        },
        {
            "name": "l2"
        }
    ],
    "scene": 0,
    "scenes": [{ "nodes": [0] }]
}
"#;

        let dir = Dir::default();
        dir.insert_asset_text(Path::new(gltf_path), gltf_str);
        let mut app = test_app(dir);
        app.update();
        let asset_server = app.world().resource::<AssetServer>().clone();
        let handle: Handle<Gltf> = asset_server.load(gltf_path);
        let handle_id = handle.id();
        app.update();
        run_app_until(&mut app, |_world| {
            let load_state = asset_server.get_load_state(handle_id).unwrap();
            if load_state.is_failed() {
                Some(())
            } else {
                None
            }
        });
        let load_state = asset_server.get_load_state(handle_id).unwrap();
        assert!(load_state.is_failed());
    }

    #[test]
    fn skin_node() {
        let gltf_path = "test.gltf";
        let app = load_gltf_into_app(
            gltf_path,
            r#"
{
    "asset": {
        "version": "2.0"
    },
    "nodes": [
        {
            "name": "skinned",
            "skin": 0,
            "children": [1, 2]
        },
        {
            "name": "joint1"
        },
        {
            "name": "joint2"
        }
    ],
    "skins": [
        {
            "inverseBindMatrices": 0,
            "joints": [1, 2]
        }
    ],
    "buffers": [
        {
            "uri" : "data:application/gltf-buffer;base64,AACAPwAAAAAAAAAAAAAAAAAAAAAAAIA/AAAAAAAAAAAAAAAAAAAAAAAAgD8AAAAAAAAAAAAAAAAAAAAAAACAPwAAgD8AAAAAAAAAAAAAAAAAAAAAAACAPwAAAAAAAAAAAAAAAAAAAAAAAIA/AAAAAAAAAAAAAIC/AAAAAAAAgD8=",
            "byteLength" : 128
        }
    ],
    "bufferViews": [
        {
            "buffer": 0,
            "byteLength": 128
        }
    ],
    "accessors": [
        {
            "bufferView" : 0,
            "componentType" : 5126,
            "count" : 2,
            "type" : "MAT4"
        }
    ],
    "scene": 0,
    "scenes": [{ "nodes": [0] }]
}
"#,
        );
        let asset_server = app.world().resource::<AssetServer>();
        let handle = asset_server.load(gltf_path);
        let gltf_root_assets = app.world().resource::<Assets<Gltf>>();
        let gltf_node_assets = app.world().resource::<Assets<GltfNode>>();
        let gltf_skin_assets = app.world().resource::<Assets<GltfSkin>>();
        let gltf_inverse_bind_matrices = app
            .world()
            .resource::<Assets<SkinnedMeshInverseBindposes>>();
        let gltf_root = gltf_root_assets.get(&handle).unwrap();

        assert_eq!(gltf_root.skins.len(), 1);
        assert_eq!(gltf_root.nodes.len(), 3);

        let skin = gltf_skin_assets.get(&gltf_root.skins[0]).unwrap();
        assert_eq!(skin.joints.len(), 2);
        assert_eq!(skin.joints[0], gltf_root.nodes[1]);
        assert_eq!(skin.joints[1], gltf_root.nodes[2]);
        assert!(gltf_inverse_bind_matrices.contains(&skin.inverse_bind_matrices));

        let skinned_node = gltf_node_assets.get(&gltf_root.nodes[0]).unwrap();
        assert_eq!(skinned_node.name, "skinned");
        assert_eq!(skinned_node.children.len(), 2);
        assert_eq!(skinned_node.skin.as_ref(), Some(&gltf_root.skins[0]));
    }
}
