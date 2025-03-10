mod pipeline;
mod render_pass;
mod ui_material_pipeline;

use bevy_core_pipeline::core_2d::graph::{Core2d, Node2d};
use bevy_core_pipeline::core_3d::graph::{Core3d, Node3d};
use bevy_core_pipeline::{core_2d::Camera2d, core_3d::Camera3d};
use bevy_hierarchy::Parent;
use bevy_render::{
    render_phase::PhaseItem, render_resource::BindGroupEntries, view::ViewVisibility,
    ExtractSchedule, Render,
};
use bevy_sprite::{SpriteAssetEvents, TextureAtlas};
pub use pipeline::*;
pub use render_pass::*;
pub use ui_material_pipeline::*;

use crate::graph::{NodeUi, SubGraphUi};
use crate::{
    texture_slice::ComputedTextureSlices, BackgroundColor, BorderColor, CalculatedClip,
    ContentSize, DefaultUiCamera, Node, Outline, Style, TargetCamera, UiImage, UiScale, Val,
};

use bevy_app::prelude::*;
use bevy_asset::{load_internal_asset, AssetEvent, AssetId, Assets, Handle};
use bevy_ecs::entity::EntityHashMap;
use bevy_ecs::prelude::*;
use bevy_math::{Mat4, Rect, URect, UVec4, Vec2, Vec3, Vec4Swizzles};
use bevy_render::{
    camera::Camera,
    color::Color,
    render_asset::RenderAssets,
    render_graph::{RenderGraph, RunGraphOnViewNode},
    render_phase::{sort_phase_system, AddRenderCommand, DrawFunctions, RenderPhase},
    render_resource::*,
    renderer::{RenderDevice, RenderQueue},
    texture::Image,
    view::{ExtractedView, ViewUniforms},
    Extract, RenderApp, RenderSet,
};
use bevy_sprite::TextureAtlasLayout;
#[cfg(feature = "bevy_text")]
use bevy_text::{PositionedGlyph, Text, TextLayoutInfo};
use bevy_transform::components::GlobalTransform;
use bevy_utils::{FloatOrd, HashMap};
use bytemuck::{Pod, Zeroable};
use std::ops::Range;

pub mod graph {
    use bevy_render::render_graph::{RenderLabel, RenderSubGraph};

    #[derive(Debug, Hash, PartialEq, Eq, Clone, RenderSubGraph)]
    pub struct SubGraphUi;

    #[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
    pub enum NodeUi {
        UiPass,
    }
}

pub const UI_SHADER_HANDLE: Handle<Shader> = Handle::weak_from_u128(13012847047162779583);

#[derive(Debug, Hash, PartialEq, Eq, Clone, SystemSet)]
pub enum RenderUiSystem {
    ExtractNode,
}

pub fn build_ui_render(app: &mut App) {
    load_internal_asset!(app, UI_SHADER_HANDLE, "ui.wgsl", Shader::from_wgsl);

    let Ok(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };

    render_app
        .init_resource::<SpecializedRenderPipelines<UiPipeline>>()
        .init_resource::<UiImageBindGroups>()
        .init_resource::<UiMeta>()
        .init_resource::<ExtractedUiNodes>()
        .allow_ambiguous_resource::<ExtractedUiNodes>()
        .init_resource::<DrawFunctions<TransparentUi>>()
        .add_render_command::<TransparentUi, DrawUi>()
        .add_systems(
            ExtractSchedule,
            (
                extract_default_ui_camera_view::<Camera2d>,
                extract_default_ui_camera_view::<Camera3d>,
                extract_uinodes.in_set(RenderUiSystem::ExtractNode),
                extract_uinode_borders,
                #[cfg(feature = "bevy_text")]
                extract_text_uinodes,
                extract_uinode_outlines,
            ),
        )
        .add_systems(
            Render,
            (
                queue_uinodes.in_set(RenderSet::Queue),
                sort_phase_system::<TransparentUi>.in_set(RenderSet::PhaseSort),
                prepare_uinodes.in_set(RenderSet::PrepareBindGroups),
            ),
        );

    // Render graph
    let ui_graph_2d = get_ui_graph(render_app);
    let ui_graph_3d = get_ui_graph(render_app);
    let mut graph = render_app.world.resource_mut::<RenderGraph>();

    if let Some(graph_2d) = graph.get_sub_graph_mut(Core2d) {
        graph_2d.add_sub_graph(SubGraphUi, ui_graph_2d);
        graph_2d.add_node(NodeUi::UiPass, RunGraphOnViewNode::new(SubGraphUi));
        graph_2d.add_node_edge(Node2d::MainPass, NodeUi::UiPass);
        graph_2d.add_node_edge(Node2d::EndMainPassPostProcessing, NodeUi::UiPass);
        graph_2d.add_node_edge(NodeUi::UiPass, Node2d::Upscaling);
    }

    if let Some(graph_3d) = graph.get_sub_graph_mut(Core3d) {
        graph_3d.add_sub_graph(SubGraphUi, ui_graph_3d);
        graph_3d.add_node(NodeUi::UiPass, RunGraphOnViewNode::new(SubGraphUi));
        graph_3d.add_node_edge(Node3d::EndMainPass, NodeUi::UiPass);
        graph_3d.add_node_edge(Node3d::EndMainPassPostProcessing, NodeUi::UiPass);
        graph_3d.add_node_edge(NodeUi::UiPass, Node3d::Upscaling);
    }
}

fn get_ui_graph(render_app: &mut App) -> RenderGraph {
    let ui_pass_node = UiPassNode::new(&mut render_app.world);
    let mut ui_graph = RenderGraph::default();
    ui_graph.add_node(NodeUi::UiPass, ui_pass_node);
    ui_graph
}

pub struct ExtractedUiNode {
    pub stack_index: u32,
    pub transform: Mat4,
    pub color: Color,
    pub rect: Rect,
    pub image: AssetId<Image>,
    pub atlas_size: Option<Vec2>,
    pub clip: Option<Rect>,
    pub flip_x: bool,
    pub flip_y: bool,
    // Camera to render this UI node to. By the time it is extracted,
    // it is defaulted to a single camera if only one exists.
    // Nodes with ambiguous camera will be ignored.
    pub camera_entity: Entity,
}

#[derive(Resource, Default)]
pub struct ExtractedUiNodes {
    pub uinodes: EntityHashMap<ExtractedUiNode>,
}

pub(crate) fn resolve_border_thickness(value: Val, parent_width: f32, viewport_size: Vec2) -> f32 {
    match value {
        Val::Auto => 0.,
        Val::Px(px) => px.max(0.),
        Val::Percent(percent) => (parent_width * percent / 100.).max(0.),
        Val::Vw(percent) => (viewport_size.x * percent / 100.).max(0.),
        Val::Vh(percent) => (viewport_size.y * percent / 100.).max(0.),
        Val::VMin(percent) => (viewport_size.min_element() * percent / 100.).max(0.),
        Val::VMax(percent) => (viewport_size.max_element() * percent / 100.).max(0.),
    }
}

pub fn extract_uinode_borders(
    mut commands: Commands,
    mut extracted_uinodes: ResMut<ExtractedUiNodes>,
    camera_query: Extract<Query<(Entity, &Camera)>>,
    default_ui_camera: Extract<DefaultUiCamera>,
    ui_scale: Extract<Res<UiScale>>,
    uinode_query: Extract<
        Query<
            (
                &Node,
                &GlobalTransform,
                &Style,
                &BorderColor,
                Option<&Parent>,
                &ViewVisibility,
                Option<&CalculatedClip>,
                Option<&TargetCamera>,
            ),
            Without<ContentSize>,
        >,
    >,
    node_query: Extract<Query<&Node>>,
) {
    let image = AssetId::<Image>::default();

    for (node, global_transform, style, border_color, parent, view_visibility, clip, camera) in
        &uinode_query
    {
        let Some(camera_entity) = camera.map(TargetCamera::entity).or(default_ui_camera.get())
        else {
            continue;
        };
        // Skip invisible borders
        if !view_visibility.get()
            || border_color.0.is_fully_transparent()
            || node.size().x <= 0.
            || node.size().y <= 0.
        {
            continue;
        }

        let ui_logical_viewport_size = camera_query
            .get(camera_entity)
            .ok()
            .and_then(|(_, c)| c.logical_viewport_size())
            .unwrap_or(Vec2::ZERO)
            // The logical window resolution returned by `Window` only takes into account the window scale factor and not `UiScale`,
            // so we have to divide by `UiScale` to get the size of the UI viewport.
            / ui_scale.0;

        // Both vertical and horizontal percentage border values are calculated based on the width of the parent node
        // <https://developer.mozilla.org/en-US/docs/Web/CSS/border-width>
        let parent_width = parent
            .and_then(|parent| node_query.get(parent.get()).ok())
            .map(|parent_node| parent_node.size().x)
            .unwrap_or(ui_logical_viewport_size.x);
        let left =
            resolve_border_thickness(style.border.left, parent_width, ui_logical_viewport_size);
        let right =
            resolve_border_thickness(style.border.right, parent_width, ui_logical_viewport_size);
        let top =
            resolve_border_thickness(style.border.top, parent_width, ui_logical_viewport_size);
        let bottom =
            resolve_border_thickness(style.border.bottom, parent_width, ui_logical_viewport_size);

        // Calculate the border rects, ensuring no overlap.
        // The border occupies the space between the node's bounding rect and the node's bounding rect inset in each direction by the node's corresponding border value.
        let max = 0.5 * node.size();
        let min = -max;
        let inner_min = min + Vec2::new(left, top);
        let inner_max = (max - Vec2::new(right, bottom)).max(inner_min);
        let border_rects = [
            // Left border
            Rect {
                min,
                max: Vec2::new(inner_min.x, max.y),
            },
            // Right border
            Rect {
                min: Vec2::new(inner_max.x, min.y),
                max,
            },
            // Top border
            Rect {
                min: Vec2::new(inner_min.x, min.y),
                max: Vec2::new(inner_max.x, inner_min.y),
            },
            // Bottom border
            Rect {
                min: Vec2::new(inner_min.x, inner_max.y),
                max: Vec2::new(inner_max.x, max.y),
            },
        ];

        let transform = global_transform.compute_matrix();

        for edge in border_rects {
            if edge.min.x < edge.max.x && edge.min.y < edge.max.y {
                extracted_uinodes.uinodes.insert(
                    commands.spawn_empty().id(),
                    ExtractedUiNode {
                        stack_index: node.stack_index,
                        // This translates the uinode's transform to the center of the current border rectangle
                        transform: transform * Mat4::from_translation(edge.center().extend(0.)),
                        color: border_color.0,
                        rect: Rect {
                            max: edge.size(),
                            ..Default::default()
                        },
                        image,
                        atlas_size: None,
                        clip: clip.map(|clip| clip.clip),
                        flip_x: false,
                        flip_y: false,
                        camera_entity,
                    },
                );
            }
        }
    }
}

pub fn extract_uinode_outlines(
    mut commands: Commands,
    mut extracted_uinodes: ResMut<ExtractedUiNodes>,
    default_ui_camera: Extract<DefaultUiCamera>,
    uinode_query: Extract<
        Query<(
            &Node,
            &GlobalTransform,
            &Outline,
            &ViewVisibility,
            Option<&CalculatedClip>,
            Option<&TargetCamera>,
        )>,
    >,
) {
    let image = AssetId::<Image>::default();
    for (node, global_transform, outline, view_visibility, maybe_clip, camera) in &uinode_query {
        let Some(camera_entity) = camera.map(TargetCamera::entity).or(default_ui_camera.get())
        else {
            continue;
        };
        // Skip invisible outlines
        if !view_visibility.get()
            || outline.color.is_fully_transparent()
            || node.outline_width == 0.
        {
            continue;
        }

        // Calculate the outline rects.
        let inner_rect = Rect::from_center_size(Vec2::ZERO, node.size() + 2. * node.outline_offset);
        let outer_rect = inner_rect.inset(node.outline_width());
        let outline_edges = [
            // Left edge
            Rect::new(
                outer_rect.min.x,
                outer_rect.min.y,
                inner_rect.min.x,
                outer_rect.max.y,
            ),
            // Right edge
            Rect::new(
                inner_rect.max.x,
                outer_rect.min.y,
                outer_rect.max.x,
                outer_rect.max.y,
            ),
            // Top edge
            Rect::new(
                inner_rect.min.x,
                outer_rect.min.y,
                inner_rect.max.x,
                inner_rect.min.y,
            ),
            // Bottom edge
            Rect::new(
                inner_rect.min.x,
                inner_rect.max.y,
                inner_rect.max.x,
                outer_rect.max.y,
            ),
        ];

        let transform = global_transform.compute_matrix();

        for edge in outline_edges {
            if edge.min.x < edge.max.x && edge.min.y < edge.max.y {
                extracted_uinodes.uinodes.insert(
                    commands.spawn_empty().id(),
                    ExtractedUiNode {
                        stack_index: node.stack_index,
                        // This translates the uinode's transform to the center of the current border rectangle
                        transform: transform * Mat4::from_translation(edge.center().extend(0.)),
                        color: outline.color,
                        rect: Rect {
                            max: edge.size(),
                            ..Default::default()
                        },
                        image,
                        atlas_size: None,
                        clip: maybe_clip.map(|clip| clip.clip),
                        flip_x: false,
                        flip_y: false,
                        camera_entity,
                    },
                );
            }
        }
    }
}

pub fn extract_uinodes(
    mut commands: Commands,
    mut extracted_uinodes: ResMut<ExtractedUiNodes>,
    texture_atlases: Extract<Res<Assets<TextureAtlasLayout>>>,
    default_ui_camera: Extract<DefaultUiCamera>,
    uinode_query: Extract<
        Query<(
            Entity,
            &Node,
            &GlobalTransform,
            &BackgroundColor,
            Option<&UiImage>,
            &ViewVisibility,
            Option<&CalculatedClip>,
            Option<&TextureAtlas>,
            Option<&TargetCamera>,
            Option<&ComputedTextureSlices>,
        )>,
    >,
) {
    for (
        entity,
        uinode,
        transform,
        color,
        maybe_image,
        view_visibility,
        clip,
        atlas,
        camera,
        slices,
    ) in uinode_query.iter()
    {
        let Some(camera_entity) = camera.map(TargetCamera::entity).or(default_ui_camera.get())
        else {
            continue;
        };
        // Skip invisible and completely transparent nodes
        if !view_visibility.get() || color.0.is_fully_transparent() {
            continue;
        }

        if let Some((image, slices)) = maybe_image.zip(slices) {
            extracted_uinodes.uinodes.extend(
                slices
                    .extract_ui_nodes(transform, uinode, color, image, clip, camera_entity)
                    .map(|e| (commands.spawn_empty().id(), e)),
            );
            continue;
        }

        let (image, flip_x, flip_y) = if let Some(image) = maybe_image {
            (image.texture.id(), image.flip_x, image.flip_y)
        } else {
            (AssetId::default(), false, false)
        };

        let (rect, atlas_size) = match atlas {
            Some(atlas) => {
                let Some(layout) = texture_atlases.get(&atlas.layout) else {
                    // Atlas not present in assets resource (should this warn the user?)
                    continue;
                };
                let mut atlas_rect = layout.textures[atlas.index];
                let mut atlas_size = layout.size;
                let scale = uinode.size() / atlas_rect.size();
                atlas_rect.min *= scale;
                atlas_rect.max *= scale;
                atlas_size *= scale;
                (atlas_rect, Some(atlas_size))
            }
            None => (
                Rect {
                    min: Vec2::ZERO,
                    max: uinode.calculated_size,
                },
                None,
            ),
        };

        extracted_uinodes.uinodes.insert(
            entity,
            ExtractedUiNode {
                stack_index: uinode.stack_index,
                transform: transform.compute_matrix(),
                color: color.0,
                rect,
                clip: clip.map(|clip| clip.clip),
                image,
                atlas_size,
                flip_x,
                flip_y,
                camera_entity,
            },
        );
    }
}

/// The UI camera is "moved back" by this many units (plus the [`UI_CAMERA_TRANSFORM_OFFSET`]) and also has a view
/// distance of this many units. This ensures that with a left-handed projection,
/// as ui elements are "stacked on top of each other", they are within the camera's view
/// and have room to grow.
// TODO: Consider computing this value at runtime based on the maximum z-value.
const UI_CAMERA_FAR: f32 = 1000.0;

// This value is subtracted from the far distance for the camera's z-position to ensure nodes at z == 0.0 are rendered
// TODO: Evaluate if we still need this.
const UI_CAMERA_TRANSFORM_OFFSET: f32 = -0.1;

#[derive(Component)]
pub struct DefaultCameraView(pub Entity);

pub fn extract_default_ui_camera_view<T: Component>(
    mut commands: Commands,
    ui_scale: Extract<Res<UiScale>>,
    query: Extract<Query<(Entity, &Camera), With<T>>>,
) {
    let scale = ui_scale.0.recip();
    for (entity, camera) in &query {
        // ignore inactive cameras
        if !camera.is_active {
            continue;
        }

        if let (
            Some(logical_size),
            Some(URect {
                min: physical_origin,
                ..
            }),
            Some(physical_size),
        ) = (
            camera.logical_viewport_size(),
            camera.physical_viewport_rect(),
            camera.physical_viewport_size(),
        ) {
            // use a projection matrix with the origin in the top left instead of the bottom left that comes with OrthographicProjection
            let projection_matrix = Mat4::orthographic_rh(
                0.0,
                logical_size.x * scale,
                logical_size.y * scale,
                0.0,
                0.0,
                UI_CAMERA_FAR,
            );
            let default_camera_view = commands
                .spawn(ExtractedView {
                    projection: projection_matrix,
                    transform: GlobalTransform::from_xyz(
                        0.0,
                        0.0,
                        UI_CAMERA_FAR + UI_CAMERA_TRANSFORM_OFFSET,
                    ),
                    view_projection: None,
                    hdr: camera.hdr,
                    viewport: UVec4::new(
                        physical_origin.x,
                        physical_origin.y,
                        physical_size.x,
                        physical_size.y,
                    ),
                    color_grading: Default::default(),
                })
                .id();
            commands.get_or_spawn(entity).insert((
                DefaultCameraView(default_camera_view),
                RenderPhase::<TransparentUi>::default(),
            ));
        }
    }
}

#[cfg(feature = "bevy_text")]
pub fn extract_text_uinodes(
    mut commands: Commands,
    mut extracted_uinodes: ResMut<ExtractedUiNodes>,
    camera_query: Extract<Query<(Entity, &Camera)>>,
    default_ui_camera: Extract<DefaultUiCamera>,
    texture_atlases: Extract<Res<Assets<TextureAtlasLayout>>>,
    ui_scale: Extract<Res<UiScale>>,
    uinode_query: Extract<
        Query<(
            &Node,
            &GlobalTransform,
            &Text,
            &TextLayoutInfo,
            &ViewVisibility,
            Option<&CalculatedClip>,
            Option<&TargetCamera>,
        )>,
    >,
) {
    for (uinode, global_transform, text, text_layout_info, view_visibility, clip, camera) in
        uinode_query.iter()
    {
        let Some(camera_entity) = camera.map(TargetCamera::entity).or(default_ui_camera.get())
        else {
            continue;
        };
        // Skip if not visible or if size is set to zero (e.g. when a parent is set to `Display::None`)
        if !view_visibility.get() || uinode.size().x == 0. || uinode.size().y == 0. {
            continue;
        }

        let scale_factor = camera_query
            .get(camera_entity)
            .ok()
            .and_then(|(_, c)| c.target_scaling_factor())
            .unwrap_or(1.0)
            * ui_scale.0;
        let inverse_scale_factor = scale_factor.recip();

        // Align the text to the nearest physical pixel:
        // * Translate by minus the text node's half-size
        //      (The transform translates to the center of the node but the text coordinates are relative to the node's top left corner)
        // * Multiply the logical coordinates by the scale factor to get its position in physical coordinates
        // * Round the physical position to the nearest physical pixel
        // * Multiply by the rounded physical position by the inverse scale factor to return to logical coordinates

        let logical_top_left = -0.5 * uinode.size();
        let physical_nearest_pixel = (logical_top_left * scale_factor).round();
        let logical_top_left_nearest_pixel = physical_nearest_pixel * inverse_scale_factor;
        let transform = Mat4::from(global_transform.affine())
            * Mat4::from_translation(logical_top_left_nearest_pixel.extend(0.));

        let mut color = Color::WHITE;
        let mut current_section = usize::MAX;
        for PositionedGlyph {
            position,
            atlas_info,
            section_index,
            ..
        } in &text_layout_info.glyphs
        {
            if *section_index != current_section {
                color = text.sections[*section_index].style.color.as_rgba_linear();
                current_section = *section_index;
            }
            let atlas = texture_atlases.get(&atlas_info.texture_atlas).unwrap();

            let mut rect = atlas.textures[atlas_info.glyph_index];
            rect.min *= inverse_scale_factor;
            rect.max *= inverse_scale_factor;
            extracted_uinodes.uinodes.insert(
                commands.spawn_empty().id(),
                ExtractedUiNode {
                    stack_index: uinode.stack_index,
                    transform: transform
                        * Mat4::from_translation(position.extend(0.) * inverse_scale_factor),
                    color,
                    rect,
                    image: atlas_info.texture.id(),
                    atlas_size: Some(atlas.size * inverse_scale_factor),
                    clip: clip.map(|clip| clip.clip),
                    flip_x: false,
                    flip_y: false,
                    camera_entity,
                },
            );
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct UiVertex {
    pub position: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 4],
    pub mode: u32,
}

#[derive(Resource)]
pub struct UiMeta {
    vertices: BufferVec<UiVertex>,
    view_bind_group: Option<BindGroup>,
}

impl Default for UiMeta {
    fn default() -> Self {
        Self {
            vertices: BufferVec::new(BufferUsages::VERTEX),
            view_bind_group: None,
        }
    }
}

pub(crate) const QUAD_VERTEX_POSITIONS: [Vec3; 4] = [
    Vec3::new(-0.5, -0.5, 0.0),
    Vec3::new(0.5, -0.5, 0.0),
    Vec3::new(0.5, 0.5, 0.0),
    Vec3::new(-0.5, 0.5, 0.0),
];

pub(crate) const QUAD_INDICES: [usize; 6] = [0, 2, 3, 0, 1, 2];

#[derive(Component)]
pub struct UiBatch {
    pub range: Range<u32>,
    pub image: AssetId<Image>,
    pub camera: Entity,
}

const TEXTURED_QUAD: u32 = 0;
const UNTEXTURED_QUAD: u32 = 1;

#[allow(clippy::too_many_arguments)]
pub fn queue_uinodes(
    extracted_uinodes: Res<ExtractedUiNodes>,
    ui_pipeline: Res<UiPipeline>,
    mut pipelines: ResMut<SpecializedRenderPipelines<UiPipeline>>,
    mut views: Query<(&ExtractedView, &mut RenderPhase<TransparentUi>)>,
    pipeline_cache: Res<PipelineCache>,
    draw_functions: Res<DrawFunctions<TransparentUi>>,
) {
    let draw_function = draw_functions.read().id::<DrawUi>();
    for (entity, extracted_uinode) in extracted_uinodes.uinodes.iter() {
        let Ok((view, mut transparent_phase)) = views.get_mut(extracted_uinode.camera_entity)
        else {
            continue;
        };

        let pipeline = pipelines.specialize(
            &pipeline_cache,
            &ui_pipeline,
            UiPipelineKey { hdr: view.hdr },
        );
        transparent_phase.add(TransparentUi {
            draw_function,
            pipeline,
            entity: *entity,
            sort_key: (
                FloatOrd(extracted_uinode.stack_index as f32),
                entity.index(),
            ),
            // batch_range will be calculated in prepare_uinodes
            batch_range: 0..0,
            dynamic_offset: None,
        });
    }
}

#[derive(Resource, Default)]
pub struct UiImageBindGroups {
    pub values: HashMap<AssetId<Image>, BindGroup>,
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_uinodes(
    mut commands: Commands,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut ui_meta: ResMut<UiMeta>,
    mut extracted_uinodes: ResMut<ExtractedUiNodes>,
    view_uniforms: Res<ViewUniforms>,
    ui_pipeline: Res<UiPipeline>,
    mut image_bind_groups: ResMut<UiImageBindGroups>,
    gpu_images: Res<RenderAssets<Image>>,
    mut phases: Query<&mut RenderPhase<TransparentUi>>,
    events: Res<SpriteAssetEvents>,
    mut previous_len: Local<usize>,
) {
    // If an image has changed, the GpuImage has (probably) changed
    for event in &events.images {
        match event {
            AssetEvent::Added { .. } |
            AssetEvent::Unused { .. } |
            // Images don't have dependencies
            AssetEvent::LoadedWithDependencies { .. } => {}
            AssetEvent::Modified { id } | AssetEvent::Removed { id } => {
                image_bind_groups.values.remove(id);
            }
        };
    }

    if let Some(view_binding) = view_uniforms.uniforms.binding() {
        let mut batches: Vec<(Entity, UiBatch)> = Vec::with_capacity(*previous_len);

        ui_meta.vertices.clear();
        ui_meta.view_bind_group = Some(render_device.create_bind_group(
            "ui_view_bind_group",
            &ui_pipeline.view_layout,
            &BindGroupEntries::single(view_binding),
        ));

        // Vertex buffer index
        let mut index = 0;
        for mut ui_phase in &mut phases {
            let mut batch_item_index = 0;
            let mut batch_image_handle = AssetId::invalid();

            for item_index in 0..ui_phase.items.len() {
                let item = &mut ui_phase.items[item_index];
                if let Some(extracted_uinode) = extracted_uinodes.uinodes.get(&item.entity) {
                    let mut existing_batch = batches.last_mut();

                    if batch_image_handle == AssetId::invalid()
                        || existing_batch.is_none()
                        || (batch_image_handle != AssetId::default()
                            && extracted_uinode.image != AssetId::default()
                            && batch_image_handle != extracted_uinode.image)
                        || existing_batch.as_ref().map(|(_, b)| b.camera)
                            != Some(extracted_uinode.camera_entity)
                    {
                        if let Some(gpu_image) = gpu_images.get(extracted_uinode.image) {
                            batch_item_index = item_index;
                            batch_image_handle = extracted_uinode.image;

                            let new_batch = UiBatch {
                                range: index..index,
                                image: extracted_uinode.image,
                                camera: extracted_uinode.camera_entity,
                            };

                            batches.push((item.entity, new_batch));

                            image_bind_groups
                                .values
                                .entry(batch_image_handle)
                                .or_insert_with(|| {
                                    render_device.create_bind_group(
                                        "ui_material_bind_group",
                                        &ui_pipeline.image_layout,
                                        &BindGroupEntries::sequential((
                                            &gpu_image.texture_view,
                                            &gpu_image.sampler,
                                        )),
                                    )
                                });

                            existing_batch = batches.last_mut();
                        } else {
                            continue;
                        }
                    } else if batch_image_handle == AssetId::default()
                        && extracted_uinode.image != AssetId::default()
                    {
                        if let Some(gpu_image) = gpu_images.get(extracted_uinode.image) {
                            batch_image_handle = extracted_uinode.image;
                            existing_batch.as_mut().unwrap().1.image = extracted_uinode.image;

                            image_bind_groups
                                .values
                                .entry(batch_image_handle)
                                .or_insert_with(|| {
                                    render_device.create_bind_group(
                                        "ui_material_bind_group",
                                        &ui_pipeline.image_layout,
                                        &BindGroupEntries::sequential((
                                            &gpu_image.texture_view,
                                            &gpu_image.sampler,
                                        )),
                                    )
                                });
                        } else {
                            continue;
                        }
                    }

                    let mode = if extracted_uinode.image != AssetId::default() {
                        TEXTURED_QUAD
                    } else {
                        UNTEXTURED_QUAD
                    };

                    let mut uinode_rect = extracted_uinode.rect;

                    let rect_size = uinode_rect.size().extend(1.0);

                    // Specify the corners of the node
                    let positions = QUAD_VERTEX_POSITIONS.map(|pos| {
                        (extracted_uinode.transform * (pos * rect_size).extend(1.)).xyz()
                    });

                    // Calculate the effect of clipping
                    // Note: this won't work with rotation/scaling, but that's much more complex (may need more that 2 quads)
                    let mut positions_diff = if let Some(clip) = extracted_uinode.clip {
                        [
                            Vec2::new(
                                f32::max(clip.min.x - positions[0].x, 0.),
                                f32::max(clip.min.y - positions[0].y, 0.),
                            ),
                            Vec2::new(
                                f32::min(clip.max.x - positions[1].x, 0.),
                                f32::max(clip.min.y - positions[1].y, 0.),
                            ),
                            Vec2::new(
                                f32::min(clip.max.x - positions[2].x, 0.),
                                f32::min(clip.max.y - positions[2].y, 0.),
                            ),
                            Vec2::new(
                                f32::max(clip.min.x - positions[3].x, 0.),
                                f32::min(clip.max.y - positions[3].y, 0.),
                            ),
                        ]
                    } else {
                        [Vec2::ZERO; 4]
                    };

                    let positions_clipped = [
                        positions[0] + positions_diff[0].extend(0.),
                        positions[1] + positions_diff[1].extend(0.),
                        positions[2] + positions_diff[2].extend(0.),
                        positions[3] + positions_diff[3].extend(0.),
                    ];

                    let transformed_rect_size =
                        extracted_uinode.transform.transform_vector3(rect_size);

                    // Don't try to cull nodes that have a rotation
                    // In a rotation around the Z-axis, this value is 0.0 for an angle of 0.0 or π
                    // In those two cases, the culling check can proceed normally as corners will be on
                    // horizontal / vertical lines
                    // For all other angles, bypass the culling check
                    // This does not properly handles all rotations on all axis
                    if extracted_uinode.transform.x_axis[1] == 0.0 {
                        // Cull nodes that are completely clipped
                        if positions_diff[0].x - positions_diff[1].x >= transformed_rect_size.x
                            || positions_diff[1].y - positions_diff[2].y >= transformed_rect_size.y
                        {
                            continue;
                        }
                    }
                    let uvs = if mode == UNTEXTURED_QUAD {
                        [Vec2::ZERO, Vec2::X, Vec2::ONE, Vec2::Y]
                    } else {
                        let atlas_extent = extracted_uinode.atlas_size.unwrap_or(uinode_rect.max);
                        if extracted_uinode.flip_x {
                            std::mem::swap(&mut uinode_rect.max.x, &mut uinode_rect.min.x);
                            positions_diff[0].x *= -1.;
                            positions_diff[1].x *= -1.;
                            positions_diff[2].x *= -1.;
                            positions_diff[3].x *= -1.;
                        }
                        if extracted_uinode.flip_y {
                            std::mem::swap(&mut uinode_rect.max.y, &mut uinode_rect.min.y);
                            positions_diff[0].y *= -1.;
                            positions_diff[1].y *= -1.;
                            positions_diff[2].y *= -1.;
                            positions_diff[3].y *= -1.;
                        }
                        [
                            Vec2::new(
                                uinode_rect.min.x + positions_diff[0].x,
                                uinode_rect.min.y + positions_diff[0].y,
                            ),
                            Vec2::new(
                                uinode_rect.max.x + positions_diff[1].x,
                                uinode_rect.min.y + positions_diff[1].y,
                            ),
                            Vec2::new(
                                uinode_rect.max.x + positions_diff[2].x,
                                uinode_rect.max.y + positions_diff[2].y,
                            ),
                            Vec2::new(
                                uinode_rect.min.x + positions_diff[3].x,
                                uinode_rect.max.y + positions_diff[3].y,
                            ),
                        ]
                        .map(|pos| pos / atlas_extent)
                    };

                    let color = extracted_uinode.color.as_linear_rgba_f32();
                    for i in QUAD_INDICES {
                        ui_meta.vertices.push(UiVertex {
                            position: positions_clipped[i].into(),
                            uv: uvs[i].into(),
                            color,
                            mode,
                        });
                    }
                    index += QUAD_INDICES.len() as u32;
                    existing_batch.unwrap().1.range.end = index;
                    ui_phase.items[batch_item_index].batch_range_mut().end += 1;
                } else {
                    batch_image_handle = AssetId::invalid();
                }
            }
        }
        ui_meta.vertices.write_buffer(&render_device, &render_queue);
        *previous_len = batches.len();
        commands.insert_or_spawn_batch(batches);
    }
    extracted_uinodes.uinodes.clear();
}
