//! Cursor and object asset loading.

use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail, ensure};
use bevy::asset::RenderAssetUsages;
use bevy::gltf::GltfAssetLabel;
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::prelude::*;
use rust_embed::RustEmbed;

use crate::config::{AppConfig, CURSOR_DEPTH};
use crate::inline::{InlineObject, RgpInlineObject};
use crate::paths::{expand_path, runtime_asset_root};

#[derive(RustEmbed)]
#[folder = "assets/objects/"]
struct EmbeddedObjects;

/// Marker for the spawned cursor model root.
#[derive(Component)]
pub struct CursorModel;

/// Loaded object source.
pub enum ObjectSource {
    /// OBJ mesh parts.
    Obj(Vec<Mesh>),
    /// glTF scene asset path.
    Gltf(String),
    /// STL, should be similar to OBJ
    Stl(Mesh),
}

impl From<ObjectSource> for InlineObject {
    fn from(val: ObjectSource) -> Self {
        InlineObject::RgpObject(match val {
            ObjectSource::Stl(mesh) => RgpInlineObject::Stl { mesh, handle: None },
            ObjectSource::Obj(meshes) => RgpInlineObject::Obj {
                meshes,
                handles: None,
            },
            ObjectSource::Gltf(asset_path) => RgpInlineObject::Gltf {
                asset_path,
                handle: None,
            },
        })
    }
}

/// Options that control object source loading.
#[derive(Clone, Copy, Debug)]
pub struct ObjectLoadOptions {
    /// Controls whether OBJ meshes are centered and scaled at load time.
    ///
    /// When enabled, each OBJ mesh is centered around its bounding-box center
    /// and scaled by the largest bounding-box axis. Disable this for generated
    /// or assembled OBJ assets whose source coordinates should be preserved.
    pub normalize: bool,
}

impl Default for ObjectLoadOptions {
    fn default() -> Self {
        Self { normalize: true }
    }
}

/// Spawns the configured cursor model.
pub fn spawn_cursor_model(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    asset_server: &AssetServer,
    app_config: &AppConfig,
) {
    let root = commands
        .spawn((
            CursorModel,
            Transform::from_xyz(0.0, 0.0, CURSOR_DEPTH),
            Visibility::Visible,
        ))
        .id();

    let base_color_texture = app_config.cursor.model.texture.as_deref().and_then(|path| {
        match load_texture_image(path) {
            Ok(image) => {
                info!("loaded cursor texture from {}", path.display());
                Some(images.add(image))
            }
            Err(error) => {
                warn!("failed to load cursor texture: {error:#}");
                None
            }
        }
    });

    let [r, g, b] = app_config.cursor.model.color;
    let material = materials.add(StandardMaterial {
        base_color: Color::srgb_u8(r, g, b),
        base_color_texture,
        emissive: LinearRgba::rgb(0.35, 0.35, 0.35),
        metallic: 0.0,
        perceptual_roughness: 0.28,
        reflectance: 0.6,
        cull_mode: None,
        ..default()
    });

    match load_object_source(app_config.cursor.model.path.as_path()) {
        Ok((source, ObjectSource::Obj(loaded_meshes))) if !loaded_meshes.is_empty() => {
            info!(
                "loaded cursor model from {} ({} mesh parts)",
                source,
                loaded_meshes.len()
            );
            commands.entity(root).with_children(|parent| {
                for mesh in loaded_meshes {
                    parent.spawn((
                        Mesh3d(meshes.add(mesh)),
                        MeshMaterial3d(material.clone()),
                        Transform::default(),
                    ));
                }
            });
        }
        Ok((source, ObjectSource::Gltf(asset_path))) => {
            info!("loading cursor model from {}", source);
            commands.entity(root).with_children(|parent| {
                parent.spawn(WorldAssetRoot(
                    asset_server.load(GltfAssetLabel::Scene(0).from_asset(asset_path)),
                ));
            });
        }
        Ok((source, ObjectSource::Stl(mesh))) => {
            info!("loaded cursor model from {source}");
            commands.entity(root).with_children(|parent| {
                parent.spawn((Mesh3d(meshes.add(mesh)), MeshMaterial3d(material.clone())));
            });
        }
        Err(error) => {
            warn!("failed to resolve cursor model: {error:#}");
            commands.entity(root).with_children(|parent| {
                parent.spawn((
                    Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
                    MeshMaterial3d(material),
                ));
            });
        }
        _ => {
            warn!("no cursor model found; using cube cursor fallback");
            commands.entity(root).with_children(|parent| {
                parent.spawn((
                    Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
                    MeshMaterial3d(material),
                ));
            });
        }
    }
}

/// Loads a base-color texture image from a path into a Bevy [`Image`].
///
/// # Errors
///
/// Returns an error if the file cannot be read or decoded.
fn load_texture_image(path: &Path) -> anyhow::Result<Image> {
    let path = expand_path(path);
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let dynamic = image::load_from_memory(&bytes)
        .with_context(|| format!("failed to decode texture {}", path.display()))?;
    // Normalize to 8-bit RGBA so the texture uses a widely supported GPU format.
    // Decoded 16-bit images would otherwise need the TEXTURE_FORMAT_16BIT_NORM feature.
    let rgba = image::DynamicImage::ImageRgba8(dynamic.into_rgba8());
    Ok(Image::from_dynamic(
        rgba,
        true,
        RenderAssetUsages::default(),
    ))
}

/// Loads an object source from a path.
///
/// # Errors
///
/// Returns an error if the asset cannot be resolved or parsed.
pub fn load_object_source(path: &Path) -> anyhow::Result<(String, ObjectSource)> {
    load_object_source_with_options(path, ObjectLoadOptions::default())
}

/// Loads an object source from a path with explicit load options.
///
/// # Errors
///
/// Returns an error if the asset cannot be resolved or parsed.
pub fn load_object_source_with_options(
    path: &Path,
    options: ObjectLoadOptions,
) -> anyhow::Result<(String, ObjectSource)> {
    let expanded_path = expand_path(path);
    let path = expanded_path.as_path();
    if path.exists() {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default();

        return match extension.as_str() {
            "stl" => load_stl_meshes_from_path(path)
                .map(|mesh| (path.display().to_string(), ObjectSource::Stl(mesh))),
            "obj" => load_obj_meshes_from_path(path, options.normalize)
                .map(|meshes| (path.display().to_string(), ObjectSource::Obj(meshes))),
            "glb" | "gltf" => {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|stem| !stem.is_empty())
                    .unwrap_or("external");
                let sanitized = stem
                    .chars()
                    .map(|c| match c {
                        'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
                        _ => '_',
                    })
                    .collect::<String>();
                let candidate = format!("objects/external/{sanitized}.{extension}");
                let asset_file = runtime_asset_root().join(&candidate);
                std::fs::create_dir_all(
                    asset_file
                        .parent()
                        .context("scene asset path has no parent directory")?,
                )?;
                std::fs::write(&asset_file, &bytes)
                    .with_context(|| format!("failed to materialize scene {}", path.display()))?;
                Ok((path.display().to_string(), ObjectSource::Gltf(candidate)))
            }
            _ => bail!("unsupported object format for {}", path.display()),
        };
    }

    let candidate = object_asset_path(path)?;
    let extension = Path::new(&candidate)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();

    if let Some(file_name) = Path::new(&candidate)
        .file_name()
        .and_then(|name| name.to_str())
        && let Some(file) = EmbeddedObjects::get(file_name)
    {
        return match extension.as_str() {
            "stl" => load_stl_meshes_from_bytes(&file.data)
                .map(|mesh| (format!("embedded:{file_name}"), ObjectSource::Stl(mesh))),
            "obj" => load_obj_meshes_from_bytes(file_name, &file.data, options.normalize)
                .map(|meshes| (format!("embedded:{file_name}"), ObjectSource::Obj(meshes))),
            "glb" | "gltf" => {
                let asset_path =
                    ensure_scene_asset_path(&candidate, Some((file_name, &file.data)))?;
                Ok((
                    format!("embedded:{file_name}"),
                    ObjectSource::Gltf(asset_path),
                ))
            }
            _ => bail!("unsupported object format for {}", candidate),
        };
    }

    match extension.as_str() {
        "stl" => load_stl_meshes_from_path(runtime_asset_root().join(&candidate).as_path())
            .or_else(|_| load_stl_meshes_from_path(path))
            .map(|mesh| (candidate.clone(), ObjectSource::Stl(mesh))),
        "obj" => load_obj_meshes_from_path(
            runtime_asset_root().join(&candidate).as_path(),
            options.normalize,
        )
        .or_else(|_| load_obj_meshes_from_path(path, options.normalize))
        .map(|meshes| (candidate.clone(), ObjectSource::Obj(meshes))),
        "glb" | "gltf" => {
            let asset_path = ensure_scene_asset_path(&candidate, None)?;
            Ok((candidate, ObjectSource::Gltf(asset_path)))
        }
        _ => bail!("unsupported object format for {}", candidate),
    }
}

/// Loads an RGP path source through a regular-file and encoded-byte boundary.
///
/// Unlike trusted application cursor assets, paths arriving from a PTY must
/// never block on a FIFO/device or read an unbounded file. GLB/GLTF inputs are
/// decoded synchronously into terminal-owned mesh data before registration.
pub(crate) fn load_rgp_object_source_with_options(
    _object_id: u32,
    path: &Path,
    options: ObjectLoadOptions,
    max_encoded_bytes: usize,
) -> anyhow::Result<(String, ObjectSource, usize, usize)> {
    let expanded_path = expand_path(path);
    let (display, extension, bytes) = if expanded_path.exists() {
        let extension = file_extension(&expanded_path)?;
        let (canonical, bytes) = read_regular_file_bounded(&expanded_path, max_encoded_bytes)?;
        (canonical.display().to_string(), extension, bytes)
    } else {
        let candidate = object_asset_path(path)?;
        let extension = file_extension(Path::new(&candidate))?;
        if let Some(file_name) = Path::new(&candidate)
            .file_name()
            .and_then(|name| name.to_str())
            && let Some(file) = EmbeddedObjects::get(file_name)
        {
            ensure!(
                file.data.len() <= max_encoded_bytes,
                "RGP object exceeds configured byte limit"
            );
            (
                format!("embedded:{file_name}"),
                extension,
                file.data.to_vec(),
            )
        } else {
            let runtime_path = runtime_asset_root().join(&candidate);
            let (canonical, bytes) = read_regular_file_bounded(&runtime_path, max_encoded_bytes)?;
            (canonical.display().to_string(), extension, bytes)
        }
    };
    let encoded_bytes = bytes.len();
    let (source, resident_bytes) =
        load_rgp_object_bytes(&display, &extension, &bytes, options, max_encoded_bytes)?;
    Ok((display, source, encoded_bytes, resident_bytes))
}

/// Loads an object source from inline bytes.
///
/// # Errors
///
/// Returns an error if the payload cannot be parsed or materialized.
pub fn load_object_source_from_bytes(
    format: &str,
    name: Option<&str>,
    bytes: &[u8],
) -> anyhow::Result<(String, ObjectSource)> {
    load_object_source_from_bytes_with_options(format, name, bytes, ObjectLoadOptions::default())
}

/// Loads an object source from inline bytes with explicit load options.
///
/// # Errors
///
/// Returns an error if the payload cannot be parsed or materialized.
pub fn load_object_source_from_bytes_with_options(
    format: &str,
    name: Option<&str>,
    bytes: &[u8],
    options: ObjectLoadOptions,
) -> anyhow::Result<(String, ObjectSource)> {
    let display_name = name.unwrap_or(match format {
        "obj" => "payload.obj",
        "stl" => "payload.stl",
        "glb" | "gltf" => "payload.glb",
        _ => "payload",
    });

    let payload_name = format!("payload:{display_name}");

    match format {
        "stl" => {
            load_stl_meshes_from_bytes(bytes).map(|mesh| (payload_name, ObjectSource::Stl(mesh)))
        }
        "obj" => load_obj_meshes_from_bytes(display_name, bytes, options.normalize)
            .map(|meshes| (payload_name, ObjectSource::Obj(meshes))),
        "glb" | "gltf" => {
            // Bevy scene loading still goes through the asset server, so payload-backed GLB/GLTF
            // assets need to be materialized under the asset root before they can be instantiated.
            let extension = if format == "gltf" { "gltf" } else { "glb" };
            let stem = Path::new(display_name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty())
                .unwrap_or("payload");
            let sanitized = stem
                .chars()
                .map(|c| match c {
                    'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
                    _ => '_',
                })
                .collect::<String>();
            let candidate = format!("objects/rgp/{sanitized}.{extension}");
            let asset_path = ensure_scene_asset_path(&candidate, Some((display_name, bytes)))?;
            Ok((payload_name, ObjectSource::Gltf(asset_path)))
        }
        _ => bail!("unsupported object format for {}", display_name),
    }
}

/// Loads an inline RGP payload into terminal-owned mesh data.
pub(crate) fn load_rgp_object_source_from_bytes_with_options(
    _object_id: u32,
    format: &str,
    name: Option<&str>,
    bytes: &[u8],
    options: ObjectLoadOptions,
    max_resident_bytes: usize,
) -> anyhow::Result<(String, ObjectSource, usize)> {
    let display_name = name.unwrap_or(match format {
        "obj" => "payload.obj",
        "stl" => "payload.stl",
        "glb" | "gltf" => "payload.glb",
        _ => "payload",
    });
    let (source, resident_bytes) =
        load_rgp_object_bytes(display_name, format, bytes, options, max_resident_bytes)?;
    Ok((format!("payload:{display_name}"), source, resident_bytes))
}

fn load_rgp_object_bytes(
    display_name: &str,
    format: &str,
    bytes: &[u8],
    options: ObjectLoadOptions,
    max_resident_bytes: usize,
) -> anyhow::Result<(ObjectSource, usize)> {
    let source = match format {
        "stl" => {
            validate_rgp_stl_budget(bytes, max_resident_bytes)?;
            ObjectSource::Stl(load_stl_meshes_from_bytes(bytes)?)
        }
        "obj" => {
            validate_rgp_obj_budget(bytes, max_resident_bytes)?;
            ObjectSource::Obj(load_obj_meshes_from_bytes(
                display_name,
                bytes,
                options.normalize,
            )?)
        }
        "glb" => {
            let validated_bytes = validate_rgp_glb_budget(bytes, max_resident_bytes)?;
            let source = ObjectSource::Obj(vec![load_rgp_glb_mesh_from_bytes(bytes)?]);
            let resident_bytes = validated_bytes.max(object_source_resident_bytes(&source));
            ensure!(
                resident_bytes <= max_resident_bytes,
                "RGP decoded object exceeds configured byte limit"
            );
            return Ok((source, resident_bytes));
        }
        "gltf" => bail!("RGP JSON glTF is not accepted; use a self-contained texture-free GLB"),
        _ => bail!("unsupported object format for {display_name}"),
    };
    let resident_bytes = object_source_resident_bytes(&source);
    ensure!(
        resident_bytes <= max_resident_bytes,
        "RGP decoded object exceeds configured byte limit"
    );
    Ok((source, resident_bytes))
}

fn checked_budget_add(total: &mut usize, bytes: usize, limit: usize) -> anyhow::Result<()> {
    *total = total
        .checked_add(bytes)
        .context("RGP decoded object byte count overflow")?;
    ensure!(
        *total <= limit,
        "RGP decoded object exceeds configured byte limit"
    );
    Ok(())
}

/// Bound the allocations `tobj` can derive from OBJ records before invoking it.
///
/// `triangulate + single_index` can turn one long polygon into many indices and
/// duplicate a vertex for each distinct face corner. Counting those records up
/// front avoids allocating the expanded mesh and only then discovering it is
/// over budget.
fn validate_rgp_obj_budget(bytes: &[u8], max_bytes: usize) -> anyhow::Result<()> {
    // tobj retains both positions and optional inline vertex colors. Charge
    // both vectors even when a particular `v` record omits RGB so a stream of
    // colored, unreferenced vertices cannot amplify before mesh validation.
    const SOURCE_VERTEX_BYTES: usize = 6 * std::mem::size_of::<f32>();
    const SOURCE_NORMAL_BYTES: usize = 3 * std::mem::size_of::<f32>();
    const SOURCE_TEXCOORD_BYTES: usize = 2 * std::mem::size_of::<f32>();
    // Conservatively cover tobj's single-index mesh, its deduplication map,
    // Bevy's converted vertex buffers, and transient source copies. The
    // render-lifetime extrusion bound is checked again on the decoded Mesh.
    const PARSED_RECORD_BYTES: usize = 256;
    const EXPANDED_CORNER_BYTES: usize = 192;

    let mut estimated = 0usize;
    for raw_line in bytes.split(|byte| *byte == b'\n') {
        let line = raw_line
            .split(|byte| *byte == b'#')
            .next()
            .unwrap_or_default();
        let mut fields = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let Some(record) = fields.next() else {
            continue;
        };
        // tobj first stores private parsed records (including malformed or
        // ignored faces) before exporting its final mesh. Bound that staging
        // collection for every nonempty directive, not just rendered data.
        checked_budget_add(&mut estimated, PARSED_RECORD_BYTES, max_bytes)?;
        match record {
            b"v" => checked_budget_add(&mut estimated, SOURCE_VERTEX_BYTES, max_bytes)?,
            b"vn" => checked_budget_add(&mut estimated, SOURCE_NORMAL_BYTES, max_bytes)?,
            b"vt" => checked_budget_add(&mut estimated, SOURCE_TEXCOORD_BYTES, max_bytes)?,
            b"f" => {
                let corners = fields.count();
                if corners < 3 {
                    continue;
                }
                let corner_bytes = corners
                    .checked_mul(EXPANDED_CORNER_BYTES)
                    .context("RGP OBJ corner byte count overflow")?;
                checked_budget_add(&mut estimated, corner_bytes, max_bytes)?;
                let index_bytes = corners
                    .saturating_sub(2)
                    .checked_mul(3)
                    .and_then(|indices| indices.checked_mul(std::mem::size_of::<u32>()))
                    .context("RGP OBJ index byte count overflow")?;
                checked_budget_add(&mut estimated, index_bytes, max_bytes)?;
            }
            b"l" | b"p" => bail!("RGP OBJ line and point records are not permitted"),
            _ => {}
        }
    }
    Ok(())
}

/// Bound STL parsing and the renderer's expanded per-face mesh before parsing.
fn validate_rgp_stl_budget(bytes: &[u8], max_bytes: usize) -> anyhow::Result<()> {
    // stl_io retains indexed vertices, faces, and a deduplication map while
    // Ratty constructs duplicated per-face Bevy buffers.
    const EXPANDED_TRIANGLE_BYTES: usize = 384;

    // Match stl_io's probe: only a valid UTF-8 first line beginning with
    // `solid ` selects its ASCII reader. Every other input is binary, where
    // the declared face count must describe the complete payload. In
    // particular, trailing bytes must not make a large binary file fall back
    // to the allocation-free ASCII estimate while stl_io still parses it as
    // binary.
    let first_line_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |index| index.saturating_add(1));
    let is_ascii = std::str::from_utf8(&bytes[..first_line_end])
        .is_ok_and(|header| header.starts_with("solid "));
    let triangle_count = if is_ascii {
        // ASCII STL contains exactly three `vertex` records per triangle.
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| {
                line.split(|byte| byte.is_ascii_whitespace())
                    .find(|field| !field.is_empty())
                    == Some(b"vertex".as_ref())
            })
            .count()
            .div_ceil(3)
    } else {
        let count = bytes.get(80..84).context("RGP binary STL header missing")?;
        let count = u32::from_le_bytes([count[0], count[1], count[2], count[3]]) as usize;
        let expected = count
            .checked_mul(50)
            .and_then(|bytes| bytes.checked_add(84))
            .context("RGP binary STL byte count overflow")?;
        ensure!(
            expected == bytes.len(),
            "RGP binary STL length does not match declared face count"
        );
        count
    };
    let resident_bytes = triangle_count
        .checked_mul(EXPANDED_TRIANGLE_BYTES)
        .context("RGP STL decoded byte count overflow")?;
    ensure!(
        resident_bytes <= max_bytes,
        "RGP decoded object exceeds configured byte limit"
    );
    Ok(())
}

/// Validate GLB allocation shape before synchronously decoding it into a Bevy
/// mesh. RGP accepts self-contained geometry only: external buffers, JSON
/// glTF, materials, and textures are rejected because their decoded lifetime
/// cannot be charged safely to the terminal's object budget.
fn validate_rgp_glb_budget(bytes: &[u8], max_bytes: usize) -> anyhow::Result<usize> {
    validate_rgp_glb_container_before_parse(bytes, max_bytes)?;
    let glb = gltf::Gltf::from_slice(bytes).context("failed to parse RGP GLB")?;
    let mut buffers = glb.document.buffers();
    let buffer = buffers.next().context("RGP GLB BIN buffer missing")?;
    let blob = glb.blob.as_ref().context("RGP GLB BIN chunk missing")?;
    ensure!(
        buffers.next().is_none()
            && matches!(buffer.source(), gltf::buffer::Source::Bin)
            && blob.len() >= buffer.length()
            && blob.len().saturating_sub(buffer.length()) <= 3,
        "RGP GLB must contain exactly one self-contained BIN buffer"
    );
    ensure!(
        glb.document.images().next().is_none()
            && glb.document.textures().next().is_none()
            && glb.document.samplers().next().is_none()
            && glb.document.materials().next().is_none()
            && glb.document.extensions_used().next().is_none(),
        "RGP GLB materials, textures, samplers, and extensions are not permitted"
    );

    // Keep the accepted GLB shape intentionally small and auditable. Bevy
    // creates meshes per primitive/use (not per unique accessor) and may
    // synthesize normals or widen index types, so a generic accessor sum is
    // not a safe allocation bound for hostile input.
    ensure!(
        glb.document.scenes().count() == 1
            && glb.document.nodes().count() == 1
            && glb.document.meshes().count() == 1
            && glb.document.animations().next().is_none()
            && glb.document.skins().next().is_none()
            && glb.document.cameras().next().is_none(),
        "RGP GLB must contain exactly one scene, node, and mesh without animation or skins"
    );
    let scene = glb
        .document
        .scenes()
        .next()
        .context("RGP GLB scene missing")?;
    let node = glb
        .document
        .nodes()
        .next()
        .context("RGP GLB node missing")?;
    let (translation, rotation, scale) = node.transform().decomposed();
    ensure!(
        scene.nodes().count() == 1
            && node.children().next().is_none()
            && node.camera().is_none()
            && node.mesh().is_some()
            && translation == [0.0, 0.0, 0.0]
            && rotation == [0.0, 0.0, 0.0, 1.0]
            && scale == [1.0, 1.0, 1.0],
        "RGP GLB scene must contain one identity-transformed non-nested mesh node"
    );
    let mesh = glb
        .document
        .meshes()
        .next()
        .context("RGP GLB mesh missing")?;
    let mut primitives = mesh.primitives();
    let primitive = primitives.next().context("RGP GLB primitive missing")?;
    ensure!(
        primitives.next().is_none()
            && primitive.mode() == gltf::mesh::Mode::Triangles
            && primitive.material().index().is_none()
            && primitive.morph_targets().next().is_none(),
        "RGP GLB must contain one unmaterialed triangle primitive without morph targets"
    );

    let position = primitive
        .get(&gltf::mesh::Semantic::Positions)
        .context("RGP GLB POSITION accessor missing")?;
    let normal = primitive
        .get(&gltf::mesh::Semantic::Normals)
        .context("RGP GLB NORMAL accessor missing")?;
    ensure!(
        primitive.attributes().count() == 2
            && position.data_type() == gltf::accessor::DataType::F32
            && normal.data_type() == gltf::accessor::DataType::F32
            && position.dimensions() == gltf::accessor::Dimensions::Vec3
            && normal.dimensions() == gltf::accessor::Dimensions::Vec3
            && position.count() == normal.count()
            && position.count() > 0
            && position.sparse().is_none()
            && normal.sparse().is_none(),
        "RGP GLB accepts only matching non-sparse float POSITION and NORMAL attributes"
    );

    let position_count = position.count();
    let mut resident_bytes = bytes.len();
    for accessor in [position, normal] {
        let accessor_bytes = accessor
            .count()
            .checked_mul(accessor.size())
            // Parsed source, Bevy CPU asset/staging, and GPU allocation.
            .and_then(|bytes| bytes.checked_mul(4))
            .context("RGP GLB vertex byte count overflow")?;
        checked_budget_add(&mut resident_bytes, accessor_bytes, max_bytes)?;
    }
    if let Some(indices) = primitive.indices() {
        ensure!(
            matches!(
                indices.data_type(),
                gltf::accessor::DataType::U16 | gltf::accessor::DataType::U32
            ) && indices.dimensions() == gltf::accessor::Dimensions::Scalar
                && indices.sparse().is_none()
                && indices.count() % 3 == 0,
            "RGP GLB indices must be non-sparse U16/U32 triangles"
        );
        let index_bytes = indices
            .count()
            .checked_mul(indices.size())
            .and_then(|bytes| bytes.checked_mul(4))
            .context("RGP GLB index byte count overflow")?;
        checked_budget_add(&mut resident_bytes, index_bytes, max_bytes)?;
    } else {
        ensure!(
            position_count % 3 == 0,
            "RGP GLB unindexed vertex count must form triangles"
        );
    }
    Ok(resident_bytes)
}

/// Decode the already validated single-primitive GLB directly from the exact
/// bytes that passed the RGP limits. No runtime path or asynchronous asset
/// lookup remains for a PTY child to replace after validation.
fn load_rgp_glb_mesh_from_bytes(bytes: &[u8]) -> anyhow::Result<Mesh> {
    let glb = gltf::Gltf::from_slice(bytes).context("failed to parse RGP GLB")?;
    let blob = glb.blob.as_deref().context("RGP GLB BIN chunk missing")?;
    let primitive = glb
        .document
        .meshes()
        .next()
        .and_then(|mesh| mesh.primitives().next())
        .context("RGP GLB primitive missing")?;
    let reader = primitive.reader(|buffer| (buffer.index() == 0).then_some(blob));
    let positions = reader
        .read_positions()
        .context("RGP GLB POSITION data missing")?
        .collect::<Vec<_>>();
    let normals = reader
        .read_normals()
        .context("RGP GLB NORMAL data missing")?
        .collect::<Vec<_>>();
    ensure!(
        positions.len() == normals.len()
            && positions
                .iter()
                .chain(&normals)
                .all(|values| values.iter().all(|value| value.is_finite())),
        "RGP GLB geometry contains invalid values"
    );
    let indices = reader
        .read_indices()
        .map(|indices| indices.into_u32().collect::<Vec<_>>());
    if let Some(indices) = indices.as_ref() {
        ensure!(
            indices
                .iter()
                .all(|index| (*index as usize) < positions.len()),
            "RGP GLB index is outside the vertex buffer"
        );
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    if let Some(indices) = indices {
        mesh.insert_indices(Indices::U32(indices));
    }
    Ok(mesh)
}

const MAX_RGP_GLB_JSON_BYTES: usize = 64 * 1024;

/// Bound the DOM that `gltf::Gltf::from_slice` can allocate before invoking
/// serde/gltf. Accepted RGP GLBs have one deliberately small geometry shape,
/// so a large JSON chunk is always hostile or outside the supported subset.
fn validate_rgp_glb_container_before_parse(bytes: &[u8], max_bytes: usize) -> anyhow::Result<()> {
    ensure!(
        bytes.len() <= max_bytes,
        "RGP GLB exceeds configured byte limit"
    );
    ensure!(bytes.len() >= 20, "RGP GLB header is truncated");
    ensure!(&bytes[..4] == b"glTF", "RGP GLB magic is invalid");
    let version = u32::from_le_bytes(bytes[4..8].try_into().expect("fixed GLB version slice"));
    ensure!(version == 2, "RGP GLB version is unsupported");
    let declared_len =
        u32::from_le_bytes(bytes[8..12].try_into().expect("fixed GLB length slice")) as usize;
    ensure!(declared_len == bytes.len(), "RGP GLB length is invalid");

    let json_len =
        u32::from_le_bytes(bytes[12..16].try_into().expect("fixed JSON length slice")) as usize;
    let json_type = u32::from_le_bytes(bytes[16..20].try_into().expect("fixed JSON type slice"));
    ensure!(json_type == 0x4E4F_534A, "RGP GLB JSON chunk is missing");
    ensure!(
        json_len <= MAX_RGP_GLB_JSON_BYTES.min(max_bytes),
        "RGP GLB JSON chunk exceeds supported allocation limit"
    );
    let json_end = 20usize
        .checked_add(json_len)
        .context("RGP GLB JSON chunk length overflow")?;
    let bin_header_end = json_end
        .checked_add(8)
        .context("RGP GLB BIN header length overflow")?;
    ensure!(
        bin_header_end <= bytes.len(),
        "RGP GLB BIN chunk is missing"
    );
    let bin_len = u32::from_le_bytes(
        bytes[json_end..json_end + 4]
            .try_into()
            .expect("fixed BIN length slice"),
    ) as usize;
    let bin_type = u32::from_le_bytes(
        bytes[json_end + 4..bin_header_end]
            .try_into()
            .expect("fixed BIN type slice"),
    );
    ensure!(bin_type == 0x004E_4942, "RGP GLB BIN chunk is missing");
    let bin_end = bin_header_end
        .checked_add(bin_len)
        .context("RGP GLB BIN chunk length overflow")?;
    ensure!(
        bin_end == bytes.len(),
        "RGP GLB must contain exactly one JSON and one BIN chunk"
    );
    Ok(())
}

fn file_extension(path: &Path) -> anyhow::Result<String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    ensure!(
        matches!(extension.as_str(), "obj" | "stl" | "glb" | "gltf"),
        "unsupported object format for {}",
        path.display()
    );
    Ok(extension)
}

fn read_regular_file_bounded(path: &Path, max_bytes: usize) -> anyhow::Result<(PathBuf, Vec<u8>)> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let path_metadata = std::fs::metadata(&canonical)
        .with_context(|| format!("failed to inspect {}", canonical.display()))?;
    ensure!(
        path_metadata.file_type().is_file(),
        "RGP path is not a regular file: {}",
        canonical.display()
    );
    let file = open_rgp_regular_file(&canonical)
        .with_context(|| format!("failed to open {}", canonical.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", canonical.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "RGP path is not a regular file: {}",
        canonical.display()
    );
    ensure!(
        metadata.len() <= max_bytes as u64,
        "RGP object exceeds configured byte limit"
    );
    let mut bytes = Vec::new();
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", canonical.display()))?;
    ensure!(
        bytes.len() <= max_bytes,
        "RGP object exceeds configured byte limit"
    );
    Ok((canonical, bytes))
}

#[cfg(unix)]
fn open_rgp_regular_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    // `O_NONBLOCK` prevents a raced FIFO from wedging the terminal between
    // the path metadata check and the descriptor-level fstat check. `O_NOFOLLOW`
    // rejects a raced final-component symlink; regular files ignore NONBLOCK.
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn open_rgp_regular_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Approximate persistent CPU/GPU source bytes retained for one decoded RGP object.
pub(crate) fn object_source_resident_bytes(source: &ObjectSource) -> usize {
    fn mesh_bytes(mesh: &Mesh) -> usize {
        let indices = match mesh.indices() {
            Some(Indices::U16(values)) => values.len().saturating_mul(std::mem::size_of::<u16>()),
            Some(Indices::U32(values)) => values.len().saturating_mul(std::mem::size_of::<u32>()),
            None => 0,
        };
        mesh.get_vertex_buffer_size().saturating_add(indices)
    }
    fn bounded_render_bytes(mesh: &Mesh) -> usize {
        let source_bytes = mesh_bytes(mesh);
        let Some(VertexAttributeValues::Float32x3(positions)) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        else {
            return source_bytes.saturating_mul(2);
        };
        let index_count = mesh.indices().map_or(0, |indices| indices.len());
        let (min_z, max_z) = positions
            .iter()
            .map(|position| position[2])
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(min, max), z| {
                (min.min(z), max.max(z))
            });
        if (max_z - min_z).abs() > 1e-4 {
            return source_bytes.saturating_mul(2);
        }

        // `extrude_mesh` clones the source, duplicates front/back vertices,
        // builds an edge-count HashMap, and can emit side vertices for every
        // triangle edge. These conservative factors include the retained
        // source plus transient and GPU copies.
        source_bytes
            .saturating_mul(2)
            .saturating_add(positions.len().saturating_mul(128))
            .saturating_add(index_count.saturating_mul(256))
    }
    match source {
        ObjectSource::Obj(meshes) => meshes
            .iter()
            .map(bounded_render_bytes)
            .fold(0usize, usize::saturating_add),
        ObjectSource::Stl(mesh) => bounded_render_bytes(mesh),
        ObjectSource::Gltf(path) => std::fs::metadata(runtime_asset_root().join(path))
            .ok()
            .and_then(|metadata| usize::try_from(metadata.len()).ok())
            .unwrap_or(0),
    }
}

/// Remove a runtime file materialized exclusively for an RGP GLB/GLTF source.
pub(crate) fn remove_rgp_materialized_source(source: &ObjectSource) {
    if let ObjectSource::Gltf(path) = source
        && Path::new(path).starts_with("objects/rgp")
    {
        let _ = std::fs::remove_file(runtime_asset_root().join(path));
    }
}

fn ensure_scene_asset_path(
    candidate: &str,
    embedded: Option<(&str, &[u8])>,
) -> anyhow::Result<String> {
    let asset_file = runtime_asset_root().join(candidate);
    if !asset_file.exists() {
        if let Some((name, bytes)) = embedded {
            std::fs::create_dir_all(
                asset_file
                    .parent()
                    .context("scene asset path has no parent directory")?,
            )?;
            std::fs::write(&asset_file, bytes)
                .with_context(|| format!("failed to restore embedded scene {}", name))?;
        } else {
            bail!("asset not found: {}", asset_file.display());
        }
    }

    Ok(candidate.to_string())
}

fn object_asset_path(path: &Path) -> anyhow::Result<String> {
    let components = path.components().collect::<Vec<_>>();
    if let Some(index) = components
        .iter()
        .position(|component| matches!(component, Component::Normal(part) if *part == "assets"))
    {
        let relative = components[index + 1..]
            .iter()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !relative.is_empty() {
            return Ok(relative.join("/"));
        }
    }

    if path.is_absolute() {
        bail!(
            "absolute path is outside the asset root: {}",
            path.display()
        );
    }

    let mut candidate = PathBuf::from(path);
    if candidate.components().count() == 1 {
        candidate = Path::new("objects").join(candidate);
    }

    let candidate = candidate
        .to_str()
        .context("asset path is not valid UTF-8")?
        .replace('\\', "/");
    Ok(candidate
        .strip_prefix("assets/")
        .unwrap_or(&candidate)
        .to_string())
}

fn load_stl_meshes_from_path(path: &Path) -> anyhow::Result<Mesh> {
    let data = std::fs::read(path)?;
    load_stl_meshes_from_bytes(&data)
}

fn load_stl_meshes_from_bytes(bytes: &[u8]) -> anyhow::Result<Mesh> {
    let mut c = Cursor::new(bytes);
    let stl = stl_io::read_stl(&mut c)?;

    // credit: bevy_stl (MIT)
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );

    let vertex_count = stl.faces.len() * 3;

    let mut positions = Vec::with_capacity(vertex_count);
    let mut normals = Vec::with_capacity(vertex_count);
    let mut indices = Vec::with_capacity(vertex_count);

    for (i, face) in stl.faces.iter().enumerate() {
        for j in 0..3 {
            let vertex = stl.vertices[face.vertices[j]];
            positions.push([vertex[0], vertex[1], vertex[2]]);
            normals.push([face.normal[0], face.normal[1], face.normal[2]]);
            indices.push((i * 3 + j) as u32);
        }
    }

    let uvs = vec![[0.0, 0.0]; vertex_count];

    mesh.insert_attribute(
        Mesh::ATTRIBUTE_POSITION,
        VertexAttributeValues::Float32x3(positions),
    );
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_NORMAL,
        VertexAttributeValues::Float32x3(normals),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, VertexAttributeValues::Float32x2(uvs));
    mesh.insert_indices(Indices::U32(indices));
    // appropriated code over

    Ok(mesh)
}

fn load_obj_meshes_from_path(path: &Path, normalize: bool) -> anyhow::Result<Vec<Mesh>> {
    let options = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ignore_lines: true,
        ignore_points: true,
    };
    let (models, _) = tobj::load_obj(path, &options)
        .with_context(|| format!("failed to read {}", path.display()))?;
    build_meshes(models, path.display().to_string(), normalize)
}

fn load_obj_meshes_from_bytes(
    name: &str,
    bytes: &[u8],
    normalize: bool,
) -> anyhow::Result<Vec<Mesh>> {
    let options = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ignore_lines: true,
        ignore_points: true,
    };
    let (models, _) = tobj::load_obj_buf(&mut Cursor::new(bytes), &options, |_path| {
        Ok((Vec::new(), Default::default()))
    })
    .with_context(|| format!("failed to read embedded {name}"))?;
    build_meshes(models, format!("embedded:{name}"), normalize)
}

fn build_meshes(
    models: Vec<tobj::Model>,
    source: String,
    normalize: bool,
) -> anyhow::Result<Vec<Mesh>> {
    let mut output = Vec::new();
    for model in models {
        let source_mesh = model.mesh;
        if source_mesh.positions.is_empty() {
            continue;
        }

        let mut positions = Vec::<[f32; 3]>::with_capacity(source_mesh.positions.len() / 3);
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for pos in source_mesh.positions.chunks_exact(3) {
            let point = Vec3::new(pos[0], pos[1], pos[2]);
            min = min.min(point);
            max = max.max(point);
            positions.push([point.x, point.y, point.z]);
        }

        if normalize {
            let center = (min + max) * 0.5;
            let extent = max - min;
            let max_extent = extent.max_element().max(1e-6);
            for p in &mut positions {
                p[0] = (p[0] - center.x) / max_extent;
                p[1] = (p[1] - center.y) / max_extent;
                p[2] = (p[2] - center.z) / max_extent;
            }
        }

        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        let position_count = positions.len();
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);

        if !source_mesh.vertex_color.is_empty() {
            let colors = source_mesh
                .vertex_color
                .chunks_exact(3)
                .map(|color| [color[0], color[1], color[2], 1.0])
                .collect::<Vec<[f32; 4]>>();
            if colors.len() == source_mesh.positions.len() / 3 {
                mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
            }
        }

        if !source_mesh.normals.is_empty() {
            let normals = source_mesh
                .normals
                .chunks_exact(3)
                .map(|normal| [normal[0], normal[1], normal[2]])
                .collect::<Vec<[f32; 3]>>();
            mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
        }

        if !source_mesh.texcoords.is_empty() {
            let uvs = source_mesh
                .texcoords
                .chunks_exact(2)
                .map(|uv| [uv[0], 1.0 - uv[1]])
                .collect::<Vec<[f32; 2]>>();
            if uvs.len() == position_count {
                mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
            }
        }

        mesh.insert_indices(Indices::U32(source_mesh.indices));
        output.push(mesh);
    }

    ensure!(!output.is_empty(), "no mesh content inside {source}");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glb_with_json_and_bin(json: &str, bin: &[u8]) -> Vec<u8> {
        let mut json = json.as_bytes().to_vec();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut bin = bin.to_vec();
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let total_len = 12usize
            .checked_add(8)
            .and_then(|len| len.checked_add(json.len()))
            .and_then(|len| {
                if bin.is_empty() {
                    Some(len)
                } else {
                    len.checked_add(8 + bin.len())
                }
            })
            .expect("test GLB length should fit");
        let mut glb = Vec::with_capacity(total_len);
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(
            &u32::try_from(total_len)
                .expect("test GLB length should fit u32")
                .to_le_bytes(),
        );
        glb.extend_from_slice(
            &u32::try_from(json.len())
                .expect("test JSON length should fit u32")
                .to_le_bytes(),
        );
        glb.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
        glb.extend_from_slice(&json);
        if !bin.is_empty() {
            glb.extend_from_slice(
                &u32::try_from(bin.len())
                    .expect("test BIN length should fit u32")
                    .to_le_bytes(),
            );
            glb.extend_from_slice(&0x004E_4942u32.to_le_bytes());
            glb.extend_from_slice(&bin);
        }
        glb
    }

    fn glb_with_json(json: &str) -> Vec<u8> {
        glb_with_json_and_bin(json, &[])
    }

    #[test]
    fn rgp_path_loader_rejects_non_regular_and_oversized_files() {
        let root = std::env::temp_dir().join(format!("ratty-rgp-path-test-{}", std::process::id()));
        let directory = root.join("special.glb");
        std::fs::create_dir_all(&directory).expect("test directory should be creatable");
        assert!(
            load_rgp_object_source_with_options(1, &directory, ObjectLoadOptions::default(), 16,)
                .is_err()
        );

        let oversized = root.join("oversized.obj");
        std::fs::write(&oversized, b"v 0 0 0\n").expect("test model should be writable");
        assert!(
            load_rgp_object_source_with_options(2, &oversized, ObjectLoadOptions::default(), 4,)
                .is_err()
        );
        let _ = std::fs::remove_file(oversized);
        let _ = std::fs::remove_dir(directory);
        let _ = std::fs::remove_dir(root);
    }

    #[cfg(unix)]
    #[test]
    fn rgp_path_loader_rejects_fifo_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        let root = std::env::temp_dir().join(format!(
            "ratty-rgp-fifo-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should follow the Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("FIFO test directory should be creatable");
        let fifo = root.join("object.obj");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes())
            .expect("temporary FIFO path must not contain NUL");
        // SAFETY: `fifo_path` is a valid, NUL-terminated path and the mode is
        // restricted to the current user.
        let status = unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) };
        assert_eq!(status, 0, "test FIFO should be creatable");

        assert!(
            load_rgp_object_source_with_options(1, &fifo, ObjectLoadOptions::default(), 1_024)
                .is_err()
        );
        let _ = std::fs::remove_file(fifo);
        let _ = std::fs::remove_dir(root);
    }

    #[test]
    fn rgp_obj_triangulation_expansion_is_rejected_before_parsing() {
        let mut obj = b"v 0 0 0\n".to_vec();
        obj.extend_from_slice(b"f");
        for _ in 0..128 {
            obj.extend_from_slice(b" 1");
        }
        obj.push(b'\n');

        assert!(validate_rgp_obj_budget(&obj, 1_024).is_err());
    }

    #[test]
    fn rgp_obj_colored_vertices_are_charged_without_faces() {
        let obj = b"v 0 0 0 1 0 0\nv 1 0 0 0 1 0\n";
        assert!(validate_rgp_obj_budget(obj, 2 * 256 + 47).is_err());
    }

    #[test]
    fn rgp_obj_one_corner_faces_cannot_amplify_parser_staging() {
        let obj = b"f 1\n".repeat(8);
        assert!(validate_rgp_obj_budget(&obj, 7 * 256).is_err());
        assert!(validate_rgp_obj_budget(b"l 1 2\n", usize::MAX).is_err());
    }

    #[test]
    fn rgp_binary_stl_with_trailing_byte_cannot_bypass_triangle_budget() {
        let triangle_count = 4u32;
        let mut stl = vec![0u8; 84 + triangle_count as usize * 50];
        stl[80..84].copy_from_slice(&triangle_count.to_le_bytes());
        stl.push(0);

        assert!(validate_rgp_stl_budget(&stl, 3 * 384).is_err());
    }

    #[test]
    fn rgp_glb_rejects_sparse_expansion_and_textures_before_bevy_load() {
        let sparse_expansion = glb_with_json(
            r#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5126,"count":1000000,"type":"VEC3"}]}"#,
        );
        assert!(validate_rgp_glb_budget(&sparse_expansion, 1_024).is_err());

        let texture = glb_with_json(
            r#"{"asset":{"version":"2.0"},"images":[{"uri":"data:image/png;base64,iVBORw0KGgo="}]}"#,
        );
        assert!(validate_rgp_glb_budget(&texture, 1_024).is_err());
    }

    #[test]
    fn rgp_glb_rejects_multiple_uri_less_buffers_before_bevy_clone() {
        let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0}],"meshes":[{"primitives":[{"attributes":{"POSITION":0,"NORMAL":1},"indices":2}]}],"buffers":[{"byteLength":78},{"byteLength":78}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":36},{"buffer":0,"byteOffset":72,"byteLength":6}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[0,0,0]},{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"},{"bufferView":2,"componentType":5123,"count":3,"type":"SCALAR"}]}"#;
        let glb = glb_with_json_and_bin(json, &[0; 78]);
        assert!(validate_rgp_glb_budget(&glb, 4_096).is_err());
    }

    #[test]
    fn rgp_glb_rejects_oversized_json_structure_before_dom_parse() {
        let nodes = (0..25_000).map(|_| "{}").collect::<Vec<_>>().join(",");
        let json = format!(r#"{{"asset":{{"version":"2.0"}},"nodes":[{nodes}]}}"#);
        assert!(json.len() > MAX_RGP_GLB_JSON_BYTES);
        let glb = glb_with_json_and_bin(&json, &[0; 4]);

        let error = validate_rgp_glb_budget(&glb, glb.len())
            .expect_err("oversized GLB JSON must fail before DOM parsing");
        assert!(error.to_string().contains("JSON chunk exceeds"));
    }
}
