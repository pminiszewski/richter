// Copyright © 2018 Cormac O'Brien
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or
// substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
// BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use std::rc::Rc;

use client::render::{self, Camera, ColorFormat, DepthFormat, Palette};
use common::bsp::{BspData, BspFace, BspModel, BspTexInfo, BspTextureMipmap, MIPLEVELS};

use cgmath::{Deg, Euler, InnerSpace, Matrix4, SquareMatrix, Vector3};
use chrono::Duration;
use failure::Error;
use flame;
use gfx::{self, CommandBuffer, Encoder, Factory, IndexBuffer, Slice};
use gfx::format::{R8, Unorm};
use gfx::handle::{Buffer, DepthStencilView, RenderTargetView, Sampler, ShaderResourceView};
use gfx::pso::{PipelineData, PipelineState};
use gfx::texture;
use gfx::traits::FactoryExt;
use gfx_device_gl::Resources;
use num::FromPrimitive;

// TODO: per-API coordinate system conversions
pub static BRUSH_VERTEX_SHADER_GLSL: &[u8] = br#"
#version 430

layout (location = 0) in vec3 a_Position;
layout (location = 1) in vec2 a_DiffuseTexcoord;
layout (location = 2) in vec2 a_LightmapTexcoord;

out vec2 f_diffuseTexcoord;
out vec2 f_lightmapTexcoord;

uniform mat4 u_Transform;

void main() {
    f_diffuseTexcoord = a_DiffuseTexcoord;
    f_lightmapTexcoord = a_LightmapTexcoord;
    gl_Position = u_Transform * vec4(-a_Position.y, a_Position.z, -a_Position.x, 1.0);
}
"#;

// TODO: figure out exact formula for blending lightstyles
pub static BRUSH_FRAGMENT_SHADER_GLSL: &[u8] = br#"
#version 430

in vec2 f_diffuseTexcoord;
in vec2 f_lightmapTexcoord;

uniform vec4 u_LightstyleValue;
uniform sampler2D u_Texture;
uniform sampler2D u_Fullbright;
uniform sampler2D u_Lightmap;

out vec4 Target0;

void main() {
    vec4 base_color = texture(u_Texture, f_diffuseTexcoord);
    vec4 lightmap = texture(u_Lightmap, f_lightmapTexcoord);

    vec4 lightmapped_color = vec4(base_color.rgb * lightmap.rrr, 1.0);

    int lightstyle_count = 0;
    float light_factor = 0.0;
    for (int i = 0; i < 4; i++) {
        if (u_LightstyleValue[i] != -1) {
            light_factor += u_LightstyleValue[i];
            lightstyle_count = i + 1;
        } else {
            break;
        }
    }

    if (lightstyle_count != 0) {
        light_factor /= lightstyle_count;
    } else {
        light_factor = 1.0;
    }

    float fullbright_factor = texture(u_Fullbright, f_diffuseTexcoord).r;

    Target0 = mix(lightmapped_color * light_factor, base_color, fullbright_factor);
}"#;

gfx_defines! {
    vertex BrushVertex {
        position: [f32; 3] = "a_Position",
        diffuse_texcoord: [f32; 2] = "a_DiffuseTexcoord",
        lightmap_texcoord: [f32; 2] = "a_LightmapTexcoord",
    }

    pipeline pipe_brush {
        vertex_buffer: gfx::VertexBuffer<BrushVertex> = (),
        transform: gfx::Global<[[f32; 4]; 4]> = "u_Transform",
        lightstyle_value: gfx::Global<[f32; 4]> = "u_LightstyleValue",
        diffuse_sampler: gfx::TextureSampler<[f32; 4]> = "u_Texture",
        fullbright_sampler: gfx::TextureSampler<f32> = "u_Fullbright",
        lightmap_sampler: gfx::TextureSampler<f32> = "u_Lightmap",
        out_color: gfx::RenderTarget<ColorFormat> = "Target0",
        out_depth: gfx::DepthTarget<DepthFormat> = gfx::preset::depth::LESS_EQUAL_WRITE,
    }
}

/// A `PipelineState` object specific to the `brush` and `world` pipelines.
pub type BrushPipelineState = PipelineState<Resources, <pipe_brush::Data<Resources> as PipelineData<Resources>>::Meta>;

/// A `PipelineData` object specific to the `brush` and `world` pipelines.
pub type BrushPipelineData = pipe_brush::Data<Resources>;

pub struct BrushRenderFace {
    pub slice: Slice<Resources>,
    pub tex_id: usize,
    pub lightmap_id: Option<usize>,
    pub light_styles: [u8; 4],
}

/// An object responsible for drawing brush models.
pub struct BrushRenderer {
    bsp_data: Rc<BspData>,

    faces: Box<[BrushRenderFace]>,
    texture_views: Box<[ShaderResourceView<Resources, [f32; 4]>]>,
    fullbright_views: Box<[ShaderResourceView<Resources, f32>]>,
    lightmap_views: Box<[ShaderResourceView<Resources, f32>]>,

    pipeline_state: BrushPipelineState,
    vertex_buffer: Buffer<Resources, BrushVertex>,
    dummy_texture: ShaderResourceView<Resources, [f32; 4]>,
    dummy_fullbright: ShaderResourceView<Resources, f32>,
    dummy_lightmap: ShaderResourceView<Resources, f32>,

    diffuse_sampler: Sampler<Resources>,
    fullbright_sampler: Sampler<Resources>,
    lightmap_sampler: Sampler<Resources>,
    color_target: RenderTargetView<Resources, ColorFormat>,
    depth_target: DepthStencilView<Resources, DepthFormat>,
}

pub fn create_pipeline_state<F>(factory: &mut F) -> Result<BrushPipelineState, Error>
where
    F: Factory<Resources>
{
    let shader_set = &factory.create_shader_set(
        BRUSH_VERTEX_SHADER_GLSL,
        BRUSH_FRAGMENT_SHADER_GLSL
    )?;

    let pipeline = factory.create_pipeline_state(
        &shader_set,
        gfx::Primitive::TriangleList,
        gfx::state::Rasterizer {
            front_face: gfx::state::FrontFace::Clockwise,
            cull_face: gfx::state::CullFace::Back,
            method: gfx::state::RasterMethod::Fill,
            offset: None,
            samples: Some(gfx::state::MultiSample),
        },
        pipe_brush::new(),
    )?;

    Ok(pipeline)
}

// FIXME: this calculation is (very slightly) off. not sure why.
fn calculate_lightmap_texcoords(
    position: Vector3<f32>,
    face: &BspFace,
    texinfo: &BspTexInfo,
) -> [f32; 2] {
    let mut s = texinfo.s_vector.dot(position) + texinfo.s_offset;
    s -= face.texture_mins[0] as f32;
    s /= face.extents[0] as f32;

    let mut t = texinfo.t_vector.dot(position) + texinfo.t_offset;
    t -= face.texture_mins[1] as f32;
    t /= face.extents[1] as f32;
    [s, t]
}

// TODO: remove collinear points in faces
// Converts a brush model face from edge-based layout to triangle list layout.
//
// The newly created `BrushVertex` vertices will be stored in `vertices`. The index of this face's
// first vertex in `vertices`, and the number of vertices pushed, will be stored in this face object
// for rendering.
pub(super) fn create_brush_render_face<F>(
    factory: &mut F,
    bsp_data: &BspData,
    face_id: usize,
    vertices: &mut Vec<BrushVertex>,
    lightmap_views: &mut Vec<ShaderResourceView<Resources, f32>>,
) -> Result<BrushRenderFace, Error>
where
    F: Factory<Resources>,
{
    let face = &bsp_data.faces()[face_id];
    let face_vert_id = vertices.len();
    let texinfo = &bsp_data.texinfo()[face.texinfo_id];
    let tex = &bsp_data.textures()[texinfo.tex_id];
    let face_edge_ids = &bsp_data.edgelist()[face.edge_id..face.edge_id + face.edge_count];
    let base_vertex_id = bsp_data.edges()[face_edge_ids[0].index].vertex_ids[face_edge_ids[0].direction as usize];
    let base_position = bsp_data.vertices()[base_vertex_id as usize];
    let base_diffuse_texcoords = [
        (base_position.dot(texinfo.s_vector) + texinfo.s_offset) / tex.width() as f32,
        (base_position.dot(texinfo.t_vector) + texinfo.t_offset) / tex.height() as f32,
    ];

    // start at 1 since we've already calculated the base vertex
    // stop at len - 1 because we use two vertices per iteration
    for i in 1..face_edge_ids.len() - 1 {
        // push the base vertex of the fan
        vertices.push(BrushVertex {
            position: base_position.into(),
            diffuse_texcoord: base_diffuse_texcoords,
            lightmap_texcoord: calculate_lightmap_texcoords(base_position, face, texinfo),
        });

        // push the ith and (i + 1)th vertices
        for v in 0..2 {
            let edge_id = &face_edge_ids[i + v];
            let vertex_id = bsp_data.edges()[edge_id.index].vertex_ids[edge_id.direction as usize];
            let position = bsp_data.vertices()[vertex_id as usize];
            let diffuse_texcoords = [
                (position.dot(texinfo.s_vector) + texinfo.s_offset) / tex.width() as f32,
                (position.dot(texinfo.t_vector) + texinfo.t_offset) / tex.height() as f32,
            ];
            vertices.push(BrushVertex {
                position: position.into(),
                diffuse_texcoord: diffuse_texcoords,
                lightmap_texcoord: calculate_lightmap_texcoords(position, face, texinfo),
            })
        }
    }

    let lightmap_w = face.extents[0] / 16 + 1;
    let lightmap_h = face.extents[1] / 16 + 1;
    let lightmap_size = lightmap_w * lightmap_h;

    let lightmap_id = if !texinfo.special {
        if let Some(ofs) = face.lightmap_id {
            let lightmap_data = &bsp_data.lightmaps()[ofs..ofs + lightmap_size as usize];
            let (_lightmap_handle, lightmap_view) = factory.create_texture_immutable_u8::<(R8, Unorm)>(
                texture::Kind::D2(lightmap_w as u16, lightmap_h as u16, texture::AaMode::Single),
                texture::Mipmap::Allocated,
                &[lightmap_data],
            )?;
            let l_id = lightmap_views.len();
            lightmap_views.push(lightmap_view);
            Some(l_id)
        } else {
            None
        }
    } else {
        None
    };

    let face_vert_count = vertices.len() - face_vert_id;

    Ok(BrushRenderFace {
        slice: Slice {
            start: 0,
            end: face_vert_count as u32,
            base_vertex: face_vert_id as u32,
            instances: None,
            buffer: IndexBuffer::Auto,
        },
        tex_id: texinfo.tex_id,
        lightmap_id,
        light_styles: face.light_styles,
    })
}

impl BrushRenderer {
    pub fn new<F>(
        bsp_model: &BspModel,
        palette: &Palette,
        factory: &mut F,
        color_target: RenderTargetView<Resources, ColorFormat>,
        depth_target: DepthStencilView<Resources, DepthFormat>,
    ) -> Result<BrushRenderer, Error>
    where
        F: Factory<Resources>,
    {
        let mut faces = Vec::new();
        let mut vertices = Vec::new();
        let mut lightmap_views = Vec::new();

        let pipeline_state = create_pipeline_state(factory)?;

        let bsp_data = bsp_model.bsp_data().clone();

        for face_id in bsp_model.face_id..bsp_model.face_id + bsp_model.face_count {
            faces.push(create_brush_render_face(
                factory,
                &bsp_data,
                face_id,
                &mut vertices,
                &mut lightmap_views
            )?);
        }

        let vertex_buffer = factory.create_vertex_buffer(&vertices);

        let mut texture_views = Vec::new();
        let mut fullbright_views = Vec::new();
        for tex in bsp_data.textures().iter() {
            let mut mipmaps = Vec::new();
            let mut fullbrights = Vec::new();
            for i in 0..MIPLEVELS {
                let (mipmap, fullbright) =
                    palette.translate(tex.mipmap(BspTextureMipmap::from_usize(i).unwrap()));
                mipmaps.push(mipmap);
                fullbrights.push(fullbright);
            }

            let (width, height) = tex.dimensions();

            let (_, texture_view) = factory
                .create_texture_immutable_u8::<ColorFormat>(
                    texture::Kind::D2(width as u16, height as u16, texture::AaMode::Single),
                    texture::Mipmap::Provided,
                    &[&mipmaps[0], &mipmaps[1], &mipmaps[2], &mipmaps[3]],
                )?;

            let (_, fullbright_view) = factory
                .create_texture_immutable_u8::<(R8, Unorm)>(
                    texture::Kind::D2(width as u16, height as u16, texture::AaMode::Single),
                    texture::Mipmap::Provided,
                    &[&fullbrights[0], &fullbrights[1], &fullbrights[2], &fullbrights[3]],
                )?;

            texture_views.push(texture_view);
            fullbright_views.push(fullbright_view);
        }

        let (_, dummy_texture) = render::create_dummy_texture(factory)?;
        let (_, dummy_fullbright) = render::create_dummy_fullbright(factory)?;
        let (_, dummy_lightmap) = render::create_dummy_lightmap(factory)?;

        Ok(BrushRenderer {
            bsp_data,
            faces: faces.into_boxed_slice(),
            pipeline_state,
            vertex_buffer,
            texture_views: texture_views.into_boxed_slice(),
            fullbright_views: fullbright_views.into_boxed_slice(),
            lightmap_views: lightmap_views.into_boxed_slice(),
            dummy_texture,
            dummy_fullbright,
            dummy_lightmap,
            diffuse_sampler: factory.create_sampler(gfx::texture::SamplerInfo::new(
                gfx::texture::FilterMethod::Scale,
                gfx::texture::WrapMode::Tile,
            )),
            fullbright_sampler: factory.create_sampler(gfx::texture::SamplerInfo::new(
                gfx::texture::FilterMethod::Scale,
                gfx::texture::WrapMode::Tile,
            )),
            lightmap_sampler: factory.create_sampler(gfx::texture::SamplerInfo::new(
                gfx::texture::FilterMethod::Bilinear,
                // gfx::texture::FilterMethod::Scale,
                gfx::texture::WrapMode::Tile,
            )),
            color_target,
            depth_target,
        })
    }

    fn create_pipeline_data(&self) -> Result<BrushPipelineData, Error>
    {
        let pipeline_data = pipe_brush::Data {
            vertex_buffer: self.vertex_buffer.clone(),
            transform: Matrix4::identity().into(),
            diffuse_sampler: (self.dummy_texture.clone(), self.diffuse_sampler.clone()),
            fullbright_sampler: (self.dummy_fullbright.clone(), self.fullbright_sampler.clone()),
            lightmap_sampler: (self.dummy_lightmap.clone(), self.lightmap_sampler.clone()),
            lightstyle_value: [0.0; 4],
            out_color: self.color_target.clone(),
            out_depth: self.depth_target.clone(),
        };

        Ok(pipeline_data)
    }

    pub fn render<C>(
        &self,
        encoder: &mut Encoder<Resources, C>,
        time: Duration,
        camera: &Camera,
        origin: Vector3<f32>,
        angles: Vector3<Deg<f32>>,
        lightstyle_values: &[f32],
    ) -> Result<(), Error>
    where
        C: CommandBuffer<Resources>,
    {
        let _guard = flame::start_guard("BrushRenderer::render");
        let mut pipeline_data = self.create_pipeline_data()?;

        for face in self.faces.iter() {
            let frame = self.bsp_data.texture_frame_for_time(face.tex_id, time);

            let model_transform = Matrix4::from_translation(Vector3::new(-origin.y, origin.z, -origin.x))
                * Matrix4::from(Euler::new(angles.x, angles.y, angles.z));
            pipeline_data.vertex_buffer = self.vertex_buffer.clone();
            pipeline_data.transform = (camera.transform() * model_transform).into();

            pipeline_data.diffuse_sampler.0 = self.texture_views[frame].clone();
            pipeline_data.fullbright_sampler.0 = self.fullbright_views[frame].clone();
            pipeline_data.lightmap_sampler.0 = match face.lightmap_id {
                Some(l_id) => self.lightmap_views[l_id].clone(),
                None => self.dummy_lightmap.clone(),
            };

            let mut lightstyle_value = [-1.0; 4];
            for i in 0..4 {
                if let Some(l) = lightstyle_values.get(face.light_styles[i] as usize) {
                    lightstyle_value[i] = *l;
                }
            }

            pipeline_data.lightstyle_value = lightstyle_value;

            encoder.draw(&face.slice, &self.pipeline_state, &pipeline_data);
        }

        Ok(())
    }
}
