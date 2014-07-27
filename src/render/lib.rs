// Copyright 2014 The Gfx-rs Developers.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! High-level, platform independent, bindless rendering API.

#![crate_name = "render"]
#![comment = "A platform independent renderer for gfx-rs."]
#![license = "ASL2"]
#![crate_type = "lib"]

#![feature(macro_rules, phase)]

#[phase(plugin, link)] extern crate log;
extern crate comm;
extern crate device;

use std::fmt::Show;
use std::vec::MoveItems;

use backend = device::dev;
use device::shade::{CreateShaderError, ProgramMeta, Vertex, Fragment, ShaderSource};
use device::target::{ClearData, TargetColor, TargetDepth, TargetStencil};
use shade::{BundleInternal, ShaderParam};
use resource::{Loaded, Pending};

pub type BufferHandle = uint;
pub type SurfaceHandle = uint;
pub type TextureHandle = uint;
pub type SamplerHandle = uint;
pub type ShaderHandle = uint;
pub type ProgramHandle = uint;
pub type EnvirHandle = uint;

pub mod mesh;
pub mod rast;
pub mod resource;
pub mod shade;
pub mod target;

pub type Token = uint;

/// Graphics state
struct State {
    frame: target::Frame,
}

/// An error that can happen when sending commands to the device. Any attempt to use the handles
/// returned here will fail.
#[deriving(Clone, Show)]
pub enum DeviceError {
    ErrorNewBuffer(BufferHandle),
    ErrorNewArrayBuffer,
    ErrorNewShader(ShaderHandle, CreateShaderError),
    ErrorNewProgram(ProgramHandle),
    ErrorNewFrameBuffer,
}


/// An error with an invalid texture or a uniform block.
#[deriving(Show)]
pub enum BundleError {
    ErrorBundleBlock(shade::VarBlock),
    ErrorBundleTexture(shade::VarTexture),
}

/// An error with a defined Mesh.
#[deriving(Show)]
pub enum MeshError {
    ErrorAttributeMissing,
    ErrorAttributeType,
}

/// An error that can happen when trying to draw.
#[deriving(Show)]
pub enum DrawError<'a> {
    ErrorProgram,
    ErrorBundle(BundleError),
    ErrorMesh(MeshError),
}

struct Dispatcher {
    /// Channel to receive device messages
    channel: Receiver<device::Reply<Token>>,
    /// Asynchronous device error queue
    errors: Vec<DeviceError>,
    /// cached meta-data for meshes and programs
    resource: resource::Cache,
}

impl Dispatcher {
    /// Make sure the resource is loaded. Optimally, we'd like this method to return
    /// the resource reference, but the borrow checker doesn't like the match over `Future`
    /// inside the body.
    fn demand(&mut self, fn_ready: |&resource::Cache| -> bool) {
        while !fn_ready(&self.resource) {
            let reply = self.channel.recv();
            match self.resource.process(reply) {
                Ok(_) => (),
                Err(e) => self.errors.push(e),
            }
        }
    }

    /// Get a guaranteed copy of a specific resource accessed by the function.
    fn get_any<R: Copy, E: Show>(&mut self, fun: <'a>|&'a resource::Cache| -> &'a resource::Future<R, E>) -> R {
        self.demand(|res| !fun(res).is_pending());
        *fun(&self.resource).unwrap()
    }

    fn get_buffer(&mut self, handle: BufferHandle) -> backend::Buffer {
        self.get_any(|res| &res.buffers[handle])
    }

    fn get_common_array_buffer(&mut self) -> backend::ArrayBuffer {
        self.get_any(|res| &res.array_buffers[0])
    }

    fn get_shader(&mut self, handle: ShaderHandle) -> backend::Shader {
        self.get_any(|res| &res.shaders[handle])
    }

    fn get_common_frame_buffer(&mut self) -> backend::FrameBuffer {
        self.get_any(|res| &res.frame_buffers[0])
    }

    fn get_texture(&mut self, handle: TextureHandle) -> backend::Texture {
        self.get_any(|res| &res.textures[handle])
    }
}

/// A renderer. Methods on this get translated into commands for the device.
pub struct Renderer {
    dispatcher: Dispatcher,
    device_tx: Sender<device::Request<Token>>,
    swap_ack: Receiver<device::Ack>,
    should_finish: comm::ShouldClose,
    /// the default FBO for drawing
    default_frame_buffer: backend::FrameBuffer,
    /// current state
    state: State,
}

impl Renderer {
    /// Create a new `Renderer` using given channels for communicating with the device. Generally,
    /// you want to use `gfx::start` instead.
    pub fn new(device_tx: Sender<device::Request<Token>>, device_rx: Receiver<device::Reply<Token>>,
            swap_rx: Receiver<device::Ack>, should_finish: comm::ShouldClose) -> Renderer {
        // Request the creation of the common array buffer and frame buffer
        let mut res = resource::Cache::new();
        res.array_buffers.push(Pending);
        res.frame_buffers.push(Pending);
        device_tx.send(device::Call(0, device::CreateArrayBuffer));
        device_tx.send(device::Call(0, device::CreateFrameBuffer));
        // Return
        Renderer {
            dispatcher: Dispatcher {
                channel: device_rx,
                errors: Vec::new(),
                resource: res,
            },
            device_tx: device_tx,
            swap_ack: swap_rx,
            should_finish: should_finish,
            default_frame_buffer: 0,
            state: State {
                frame: target::Frame::new(),
            },
        }
    }

    /// Ask the device to do something for us
    fn cast(&self, msg: device::CastRequest) {
        self.device_tx.send(device::Cast(msg));
    }

    /// Whether rendering should stop completely.
    pub fn should_finish(&self) -> bool {
        self.should_finish.check()
    }

    /// Iterate over any errors that have been raised by the device when trying to issue commands
    /// since the last time this method was called.
    pub fn errors(&mut self) -> MoveItems<DeviceError> {
        let errors = self.dispatcher.errors.clone();
        self.dispatcher.errors.clear();
        errors.move_iter()
    }

    /// Clear the `Frame` as the `ClearData` specifies.
    pub fn clear(&mut self, data: ClearData, frame: target::Frame) {
        self.bind_frame(&frame);
        self.cast(device::Clear(data));
    }

    /// Draw `slice` of `mesh` into `frame`, using a `bundle` of shader program and parameters, and
    /// a given draw state.
    pub fn draw<'a, L, T: shade::ShaderParam<L>>(&'a mut self, mesh: &mesh::Mesh, slice: mesh::Slice, frame: target::Frame,
            bundle: &shade::ShaderBundle<L, T>, state: rast::DrawState) -> Result<(), DrawError<'a>> {
        // demand resources. This section needs the mutable self, so we are unable to do this
        // after we get a reference to ether the `Environment` or the `ProgramMeta`
        self.prebind_mesh(mesh, &slice);
        self.prebind_bundle(bundle);
        self.dispatcher.demand(|res| !res.programs[bundle.get_program()].is_pending());
        // bind state
        self.cast(device::SetPrimitiveState(state.primitive));
        self.cast(device::SetDepthStencilState(state.depth, state.stencil,
            state.primitive.get_cull_mode()));
        self.cast(device::SetBlendState(state.blend));
        // bind array buffer
        let vao = self.dispatcher.get_common_array_buffer();
        self.cast(device::BindArrayBuffer(vao));
        // bind output frame
        self.bind_frame(&frame);
        // bind shaders
        let program = match self.dispatcher.resource.programs[bundle.get_program()] {
            resource::Pending => fail!("Program is not loaded yet"),
            resource::Loaded(ref p) => p,
            resource::Failed(_) => return Err(ErrorProgram),
        };
        match self.bind_shader_bundle(program, bundle) {
            Ok(_) => (),
            Err(e) => return Err(ErrorBundle(e)),
        }
        // bind vertex attributes
        match self.bind_mesh(mesh, program) {
            Ok(_) => (),
            Err(e) => return Err(ErrorMesh(e)),
        }
        // draw
        match slice {
            mesh::VertexSlice(start, end) => {
                self.cast(device::Draw(start, end));
            },
            mesh::IndexSlice(handle, start, end) => {
                let buf = *self.dispatcher.resource.buffers[handle].unwrap();
                self.cast(device::BindIndex(buf));
                self.cast(device::DrawIndexed(start, end));
            },
        }
        Ok(())
    }

    /// Finish rendering a frame. Waits for a frame to be finished drawing, as specified by the
    /// queue size passed to `gfx::start`.
    pub fn end_frame(&self) {
        self.device_tx.send(device::SwapBuffers);
        self.swap_ack.recv();  //wait for acknowlegement
    }

    /// Create a new program from the given vertex and fragment shaders.
    pub fn create_program(&mut self, vs_src: ShaderSource, fs_src: ShaderSource) -> ProgramHandle {
        let ds = &mut self.dispatcher;
        let id = ds.resource.shaders.len();
        ds.resource.shaders.push(Pending);
        ds.resource.shaders.push(Pending);
        self.device_tx.send(device::Call(id + 0, device::CreateShader(Vertex, vs_src)));
        self.device_tx.send(device::Call(id + 1, device::CreateShader(Fragment, fs_src)));
        let h_vs = ds.get_shader(id + 0);
        let h_fs = ds.get_shader(id + 1);
        let token = ds.resource.programs.len();
        self.device_tx.send(device::Call(token, device::CreateProgram(vec![h_vs, h_fs])));
        ds.resource.programs.push(Pending);
        token
    }

    /// Create a new buffer on the device, which can be used to store vertex or uniform data.
    pub fn create_buffer<T: Send>(&mut self, data: Option<Vec<T>>) -> BufferHandle {
        let bufs = &mut self.dispatcher.resource.buffers;
        let token = bufs.len();
        let blob = data.map(|v| (box v) as Box<device::Blob + Send>);
        self.device_tx.send(device::Call(token, device::CreateBuffer(blob)));
        bufs.push(Pending);
        token
    }

    pub fn create_mesh<T: mesh::VertexFormat + Send>(&mut self, data: Vec<T>) -> mesh::Mesh {
        let nv = data.len();
        debug_assert!(nv < 0x10000);
        let buf = self.create_buffer(Some(data));
        mesh::Mesh::from::<T>(buf, nv as mesh::VertexCount)
    }

    pub fn create_texture(&mut self, info: device::tex::TextureInfo) -> TextureHandle {
        let texs = &mut self.dispatcher.resource.textures;
        let token = texs.len();
        self.device_tx.send(device::Call(token, device::CreateTexture(info)));
        texs.push(Pending);
        token
    }

    pub fn create_sampler(&mut self, info: device::tex::SamplerInfo) -> SamplerHandle {
        let sams = &mut self.dispatcher.resource.samplers;
        let token = sams.len();
        self.device_tx.send(device::Call(token, device::CreateSampler(info)));
        sams.push(Pending);
        token
    }

    pub fn bundle_program<'a, L, T: shade::ShaderParam<L>>(&'a mut self, prog: ProgramHandle, data: T)
            -> Result<shade::ShaderBundle<L, T>, shade::ParameterLinkError<'a>> {
        self.dispatcher.demand(|res| !res.programs[prog].is_pending());
        match self.dispatcher.resource.programs[prog] {
            Loaded(ref m) => {
                let mut sink = shade::MetaSink::new(m.clone());
                match data.create_link(&mut sink) {
                    Ok(link) => match sink.complete() {
                        Ok(_) => Ok(BundleInternal::new(
                            None::<&shade::ShaderBundle<L, T>>, // a workaround to specify the type
                            prog, data, link)),
                        Err(e) => Err(shade::ErrorMissingParameter(e)),
                    },
                    Err(e) => Err(shade::ErrorUnusedParameter(e)),
                }
            },
            _ => Err(shade::ErrorBadProgram),
        }
    }

    pub fn update_buffer_vec<T: Send>(&mut self, handle: BufferHandle, data: Vec<T>) {
        let buf = self.dispatcher.get_buffer(handle);
        self.cast(device::UpdateBuffer(buf, (box data) as Box<device::Blob + Send>));
    }

    pub fn update_buffer_struct<T: device::Blob+Send>(&mut self, handle: BufferHandle, data: T) {
        let buf = self.dispatcher.get_buffer(handle);
        self.cast(device::UpdateBuffer(buf, (box data) as Box<device::Blob + Send>));
    }

    pub fn update_texture<T: Send>(&mut self, handle: TextureHandle,
                                   info: device::tex::ImageInfo, data: Vec<T>) {
        let tex = self.dispatcher.get_texture(handle);
        self.cast(device::UpdateTexture(tex, info, (box data) as Box<device::Blob + Send>));
    }

    /// Make sure all the mesh buffers are successfully created/loaded
    fn prebind_mesh(&mut self, mesh: &mesh::Mesh, slice: &mesh::Slice) {
        for at in mesh.attributes.iter() {
            self.dispatcher.get_buffer(at.buffer);
        }
        match *slice {
            mesh::IndexSlice(handle, _, _) =>
                self.dispatcher.get_buffer(handle),
            _ => 0,
        };
    }

    fn prebind_bundle<L, T: shade::ShaderParam<L>>(&mut self, bundle: &shade::ShaderBundle<L, T>) {
        let dp = &mut self.dispatcher;
        // buffers pass
        bundle.bind(|_, _| {
        }, |_, buf| {
            dp.demand(|res| !res.buffers[buf].is_pending());
        }, |_, _| {

        });
        // texture pass
        bundle.bind(|_, _| {
        }, |_, _| {
        }, |_, (tex, sam)| {
            dp.demand(|res| !res.textures[tex].is_pending());
            match sam {
                Some(sam) => dp.demand(|res| !res.samplers[sam].is_pending()),
                None => (),
            }
        });
    }

    fn bind_frame(&mut self, frame: &target::Frame) {
        if frame.is_default() {
            // binding the default FBO, not touching our common one
            self.cast(device::BindFrameBuffer(self.default_frame_buffer));
        } else {
            let fbo = self.dispatcher.get_common_frame_buffer();
            self.cast(device::BindFrameBuffer(fbo));
            for (i, (cur, new)) in self.state.frame.colors.iter().zip(frame.colors.iter()).enumerate() {
                if *cur != *new {
                    self.cast(device::BindTarget(TargetColor(i as u8), *new));
                }
            }
            if self.state.frame.depth != frame.depth {
                self.cast(device::BindTarget(TargetDepth, frame.depth));
            }
            if self.state.frame.stencil != frame.stencil {
                self.cast(device::BindTarget(TargetStencil, frame.stencil));
            }
            self.state.frame = *frame;
        }
    }

    fn bind_shader_bundle<L, T: shade::ShaderParam<L>>(&self, meta: &ProgramMeta,
            bundle: &shade::ShaderBundle<L, T>) -> Result<(), BundleError> {
        self.cast(device::BindProgram(meta.name));
        let mut block_slot   = 0u as device::UniformBufferSlot;
        let mut texture_slot = 0u as device::TextureSlot;
        let mut block_fail   = None::<shade::VarBlock>;
        let mut texture_fail = None::<shade::VarTexture>;
        bundle.bind(|uv, value| {
            self.cast(device::BindUniform(meta.uniforms[uv as uint].location, value));
        }, |bv, handle| {
            match self.dispatcher.resource.buffers[handle] {
                Loaded(block) => {
                    self.cast(device::BindUniformBlock(meta.name,
                        block_slot as device::UniformBufferSlot,
                        bv as device::UniformBlockIndex,
                        block));
                    block_slot += 1;
                },
                _ => {block_fail = Some(bv)},
            }
        }, |tv, (tex_handle, sampler)| {
            let sam = sampler.map(|sam| *self.dispatcher.resource.samplers[sam].unwrap());
            match self.dispatcher.resource.textures[tex_handle] {
                Loaded(tex) => {
                    self.cast(device::BindUniform(
                        meta.textures[tv as uint].location,
                        device::shade::ValueI32(texture_slot as i32)
                        ));
                    self.cast(device::BindTexture(texture_slot, tex, sam));
                    texture_slot += 1;
                },
                _ => {texture_fail = Some(tv)},
            }
        });
        match (block_fail, texture_fail) {
            (Some(bv), _) => Err(ErrorBundleBlock(bv)),
            (_, Some(tv)) => Err(ErrorBundleTexture(tv)),
            (None, None)  => Ok(()),
        }
    }

    fn bind_mesh(&self, mesh: &mesh::Mesh, prog: &ProgramMeta) -> Result<(), MeshError> {
        for sat in prog.attributes.iter() {
            match mesh.attributes.iter().find(|a| a.name.as_slice() == sat.name.as_slice()) {
                Some(vat) => match vat.elem_type.is_compatible(sat.base_type) {
                    Ok(_) => self.cast(device::BindAttribute(
                        sat.location as device::AttributeSlot,
                        *self.dispatcher.resource.buffers[vat.buffer].unwrap(),
                        vat.elem_count, vat.elem_type, vat.stride, vat.offset)),
                    Err(_) => return Err(ErrorAttributeType)
                },
                None => return Err(ErrorAttributeMissing)
            }
        }
        Ok(())
    }
}
