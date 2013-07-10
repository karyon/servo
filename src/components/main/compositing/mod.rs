/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use platform::{Application, Window};
use script::dom::event::{Event, ClickEvent, MouseDownEvent, MouseUpEvent, ResizeEvent};
use script::script_task::{LoadMsg, NavigateMsg, SendEventMsg};
use script::layout_interface::{LayoutChan, RouteScriptMsg};
use windowing::{ApplicationMethods, WindowMethods, WindowMouseEvent, WindowClickEvent};
use windowing::{WindowMouseDownEvent, WindowMouseUpEvent};


use servo_msg::compositor_msg::{RenderListener, LayerBuffer, LayerBufferSet, RenderState};
use servo_msg::compositor_msg::{ReadyState, ScriptListener};
use servo_msg::constellation_msg::{CompositorAck, ConstellationChan};
use servo_msg::constellation_msg;
use gfx::render_task::{RenderChan, ReRenderMsg};

use azure::azure_hl::{DataSourceSurface, DrawTarget, SourceSurfaceMethods, current_gl_context};
use azure::azure::AzGLContext;
use std::cell::Cell;
use std::comm;
use std::comm::{Chan, SharedChan, Port};
use std::num::Orderable;
use std::task;
use extra::uv_global_loop;
use extra::timer;
use geom::matrix::identity;
use geom::point::Point2D;
use geom::size::Size2D;
use geom::rect::Rect;
use layers::layers::{ARGB32Format, ContainerLayer, ContainerLayerKind, Format};
use layers::layers::{ImageData, WithDataFn};
use layers::layers::{TextureLayerKind, TextureLayer, TextureManager};
use layers::rendergl;
use layers::scene::Scene;
use servo_util::{time, url};
use servo_util::time::profile;
use servo_util::time::ProfilerChan;

use extra::arc;
pub use windowing;

use extra::time::precise_time_s;
use compositing::quadtree::Quadtree;
mod quadtree;

/// The implementation of the layers-based compositor.
#[deriving(Clone)]
pub struct CompositorChan {
    /// A channel on which messages can be sent to the compositor.
    chan: SharedChan<Msg>,
}

/// Implementation of the abstract `ScriptListener` interface.
impl ScriptListener for CompositorChan {

    fn set_ready_state(&self, ready_state: ReadyState) {
        let msg = ChangeReadyState(ready_state);
        self.chan.send(msg);
    }

}

/// Implementation of the abstract `RenderListener` interface.
impl RenderListener for CompositorChan {

    fn get_gl_context(&self) -> AzGLContext {
        let (port, chan) = comm::stream();
        self.chan.send(GetGLContext(chan));
        port.recv()
    }

    fn paint(&self, id: uint, layer_buffer_set: arc::ARC<LayerBufferSet>, new_size: Size2D<uint>) {
        self.chan.send(Paint(id, layer_buffer_set, new_size))
    }

    fn new_layer(&self, page_size: Size2D<uint>, tile_size: uint) {
        self.chan.send(NewLayer(page_size, tile_size))
    }
    fn resize_layer(&self, page_size: Size2D<uint>) {
        self.chan.send(ResizeLayer(page_size))
    }
    fn delete_layer(&self) {
        self.chan.send(DeleteLayer)
    }

    fn set_render_state(&self, render_state: RenderState) {
        self.chan.send(ChangeRenderState(render_state))
    }
}

impl CompositorChan {

    pub fn new(chan: Chan<Msg>) -> CompositorChan {
        CompositorChan {
            chan: SharedChan::new(chan),
        }
    }

    pub fn send(&self, msg: Msg) {
        self.chan.send(msg);
    }

    pub fn get_size(&self) -> Size2D<int> {
        let (port, chan) = comm::stream();
        self.chan.send(GetSize(chan));
        port.recv()
    }
}

/// Messages to the compositor.
pub enum Msg {
    /// Requests that the compositor shut down.
    Exit,
    /// Requests the window size
    GetSize(Chan<Size2D<int>>),
    /// Requests the compositors GL context.
    GetGLContext(Chan<AzGLContext>),

    // TODO: Attach layer ids and epochs to these messages
    /// Alerts the compositor that there is a new layer to be rendered.
    NewLayer(Size2D<uint>, uint),
    /// Alerts the compositor that the current layer has changed size.
    ResizeLayer(Size2D<uint>),
    /// Alerts the compositor that the current layer has been deleted.
    DeleteLayer,

    /// Requests that the compositor paint the given layer buffer set for the given page size.
    Paint(uint, arc::ARC<LayerBufferSet>, Size2D<uint>),
    /// Alerts the compositor to the current status of page loading.
    ChangeReadyState(ReadyState),
    /// Alerts the compositor to the current status of rendering.
    ChangeRenderState(RenderState),
    /// Sets the channel to the current layout and render tasks, along with their id
    SetLayoutRenderChans(LayoutChan, RenderChan , uint, ConstellationChan)
}

/// Azure surface wrapping to work with the layers infrastructure.
struct AzureDrawTargetImageData {
    draw_target: DrawTarget,
    data_source_surface: DataSourceSurface,
    size: Size2D<uint>,
}

impl ImageData for AzureDrawTargetImageData {
    fn size(&self) -> Size2D<uint> {
        self.size
    }
    fn stride(&self) -> uint {
        self.data_source_surface.stride() as uint
    }
    fn format(&self) -> Format {
        // FIXME: This is not always correct. We should query the Azure draw target for the format.
        ARGB32Format
    }
    fn with_data(&self, f: WithDataFn) {
        do self.data_source_surface.with_data |data| {
            f(data);
        }
    }
}

pub struct CompositorTask {
    port: Port<Msg>,
    profiler_chan: ProfilerChan,
    shutdown_chan: SharedChan<()>,
}

impl CompositorTask {
    pub fn new(port: Port<Msg>,
               profiler_chan: ProfilerChan,
               shutdown_chan: Chan<()>)
               -> CompositorTask {
        CompositorTask {
            port: port,
            profiler_chan: profiler_chan,
            shutdown_chan: SharedChan::new(shutdown_chan),
        }
    }

    /// Starts the compositor, which listens for messages on the specified port. 
    pub fn create(port: Port<Msg>,
                                  profiler_chan: ProfilerChan,
                                  shutdown_chan: Chan<()>) {
        let port = Cell::new(port);
        let shutdown_chan = Cell::new(shutdown_chan);
        do on_osmain {
            let compositor_task = CompositorTask::new(port.take(),
                                                      profiler_chan.clone(),
                                                      shutdown_chan.take());
            debug!("preparing to enter main loop");
            compositor_task.run_main_loop();
        };
    }

    fn run_main_loop(&self) {
        let app: Application = ApplicationMethods::new();
        let window: @mut Window = WindowMethods::new(&app);

        // Create an initial layer tree.
        //
        // TODO: There should be no initial layer tree until the renderer creates one from the display
        // list. This is only here because we don't have that logic in the renderer yet.
        let context = rendergl::init_render_context();
        let root_layer = @mut ContainerLayer();
        let window_size = window.size();
        let scene = @mut Scene(ContainerLayerKind(root_layer), window_size, identity());
        let done = @mut false;
        let recomposite = @mut false;

        // FIXME: This should not be a separate offset applied after the fact but rather should be
        // applied to the layers themselves on a per-layer basis. However, this won't work until scroll
        // positions are sent to content.
        let world_offset = @mut Point2D(0f32, 0f32);
        let page_size = @mut Size2D(0f32, 0f32);
        let window_size = @mut Size2D(window_size.width as int,
                                      window_size.height as int);

        // Keeps track of the current zoom factor
        let world_zoom = @mut 1f32;
        // Keeps track of local zoom factor. Reset to 1 after a rerender event.
        let local_zoom = @mut 1f32;
        // Channel to the current renderer.
        // FIXME: This probably shouldn't be stored like this.

        let render_chan: @mut Option<RenderChan> = @mut None;
        let pipeline_id: @mut Option<uint> = @mut None;

        // Quadtree for this layer
        // FIXME: This should be one-per-layer
        let quadtree: @mut Option<Quadtree<~LayerBuffer>> = @mut None;
        
        // Keeps track of if we have performed a zoom event and how recently.
        let zoom_action = @mut false;
        let zoom_time = @mut 0f;


        let ask_for_tiles: @fn() = || {
            match *quadtree {
                Some(ref mut quad) => {
                    let valid = |tile: &~LayerBuffer| -> bool {
                        tile.resolution == *world_zoom
                    };
                    let (tile_request, redisplay) = quad.get_tile_rects(Rect(Point2D(world_offset.x as int,
                                                                                     world_offset.y as int),
                                                                             *window_size), valid, *world_zoom);

                    if !tile_request.is_empty() {
                        match *render_chan {
                            Some(ref chan) => {
                                chan.send(ReRenderMsg(tile_request, *world_zoom));
                            }
                            _ => {
                                println("Warning: Compositor: Cannot send tile request, no render chan initialized");
                            }
                        }
                    } else if redisplay {
                        // TODO: move display code to its own closure and call that here
                    }
                }
                _ => {
                    fail!("Compositor: Tried to ask for tiles without an initialized quadtree");
                }
            }
        };

        let update_layout_callbacks: @fn(LayoutChan) = |layout_chan: LayoutChan| {
            let layout_chan_clone = layout_chan.clone();
            do window.set_navigation_callback |direction| {
                let direction = match direction {
                    windowing::Forward => constellation_msg::Forward,
                    windowing::Back => constellation_msg::Back,
                };
                layout_chan_clone.send(RouteScriptMsg(NavigateMsg(direction)));
            }

            let layout_chan_clone = layout_chan.clone();
            // Hook the windowing system's resize callback up to the resize rate limiter.
            do window.set_resize_callback |width, height| {
                let new_size = Size2D(width as int, height as int);
                if *window_size != new_size {
                    debug!("osmain: window resized to %ux%u", width, height);
                    *window_size = new_size;
                    layout_chan_clone.send(RouteScriptMsg(SendEventMsg(ResizeEvent(width, height))));
                } else {
                    debug!("osmain: dropping window resize since size is still %ux%u", width, height);
                }
            }

            let layout_chan_clone = layout_chan.clone();

            // When the user enters a new URL, load it.
            do window.set_load_url_callback |url_string| {
                debug!("osmain: loading URL `%s`", url_string);
                layout_chan_clone.send(RouteScriptMsg(LoadMsg(url::make_url(url_string.to_str(), None))));
            }

            let layout_chan_clone = layout_chan.clone();

            // When the user triggers a mouse event, perform appropriate hit testing
            do window.set_mouse_callback |window_mouse_event: WindowMouseEvent| {
                let event: Event;
                let world_mouse_point = |layer_mouse_point: Point2D<f32>| {
                    layer_mouse_point + *world_offset
                };
                match window_mouse_event {
                    WindowClickEvent(button, layer_mouse_point) => {
                        event = ClickEvent(button, world_mouse_point(layer_mouse_point));
                    }
                    WindowMouseDownEvent(button, layer_mouse_point) => {
                        event = MouseDownEvent(button, world_mouse_point(layer_mouse_point));

                    }
                    WindowMouseUpEvent(button, layer_mouse_point) => {
                        
                        // FIXME: this should happen on a scroll/zoom event instead,
                        // but is here temporarily to prevent request floods to the renderer
                        ask_for_tiles();

                        event = MouseUpEvent(button, world_mouse_point(layer_mouse_point));
                    }
                }
                layout_chan_clone.send(RouteScriptMsg(SendEventMsg(event)));
            }
        };


        let check_for_messages: @fn(&Port<Msg>) = |port: &Port<Msg>| {
            // Handle messages
            while port.peek() {
                match port.recv() {
                    Exit => *done = true,

                    ChangeReadyState(ready_state) => window.set_ready_state(ready_state),
                    ChangeRenderState(render_state) => window.set_render_state(render_state),

                    SetLayoutRenderChans(new_layout_chan,
                                         new_render_chan,
                                         new_pipeline_id,
                                         response_chan) => {
                        update_layout_callbacks(new_layout_chan);
                        *render_chan = Some(new_render_chan);
                        *pipeline_id = Some(new_pipeline_id);
                        response_chan.send(CompositorAck(new_pipeline_id));
                    }

                    GetSize(chan) => {
                        let size = window.size();
                        chan.send(Size2D(size.width as int, size.height as int));
                    }

                    GetGLContext(chan) => chan.send(current_gl_context()),
                    
                    NewLayer(new_size, tile_size) => {
                        *page_size = Size2D(new_size.width as f32, new_size.height as f32);
                        *quadtree = Some(Quadtree::new(0, 0, new_size.width, new_size.height, tile_size));
                        ask_for_tiles();
                        
                    }
                    ResizeLayer(new_size) => {
                        *page_size = Size2D(new_size.width as f32, new_size.height as f32);
                        // TODO: update quadtree, ask for tiles
                    }
                    DeleteLayer => {
                        // TODO: create secondary layer tree, keep displaying until new tiles come in
                    }

                    Paint(id, new_layer_buffer_set, new_size) => {
                        match *pipeline_id {
                            Some(pipeline_id) => if id != pipeline_id { loop; },
                            None => { loop; },
                        }
                            
                        debug!("osmain: received new frame");

                        let quad;
                        match *quadtree {
                            Some(ref mut q) => quad = q,
                            None => fail!("Compositor: given paint command with no quadtree initialized"),
                        }

                        *page_size = Size2D(new_size.width as f32, new_size.height as f32);

                        let new_layer_buffer_set = new_layer_buffer_set.get();
                        for new_layer_buffer_set.buffers.iter().advance |buffer| {
                            // FIXME: Don't copy the buffers here
                            quad.add_tile(buffer.screen_pos.origin.x, buffer.screen_pos.origin.y,
                                          *world_zoom, ~buffer.clone());
                        }
                        

                        // Iterate over the children of the container layer.
                        let mut current_layer_child = root_layer.first_child;
                        
                        let all_tiles = quad.get_all_tiles();
                        for all_tiles.iter().advance |buffer| {
                            let width = buffer.screen_pos.size.width as uint;
                            let height = buffer.screen_pos.size.height as uint;
                            debug!("osmain: compositing buffer rect %?", &buffer.rect);
                            
                            // Find or create a texture layer.
                            let texture_layer;
                            current_layer_child = match current_layer_child {
                                None => {
                                    debug!("osmain: adding new texture layer");
                                    texture_layer = @mut TextureLayer::new(@buffer.draw_target.clone() as @TextureManager,
                                                                           buffer.screen_pos.size);
                                    root_layer.add_child(TextureLayerKind(texture_layer));
                                    None
                                }
                                Some(TextureLayerKind(existing_texture_layer)) => {
                                    texture_layer = existing_texture_layer;
                                    texture_layer.manager = @buffer.draw_target.clone() as @TextureManager;

                                    // Move on to the next sibling.
                                    do current_layer_child.get().with_common |common| {
                                        common.next_sibling
                                    }
                                }
                                Some(_) => fail!(~"found unexpected layer kind"),
                            };

                            let origin = buffer.rect.origin;
                            let origin = Point2D(origin.x as f32, origin.y as f32);

                            // Set the layer's transform.
                            let transform = identity().translate(origin.x * *world_zoom, origin.y * *world_zoom, 0.0);
                            let transform = transform.scale(width as f32 * *world_zoom / buffer.resolution, height as f32 * *world_zoom / buffer.resolution, 1.0);
                            texture_layer.common.set_transform(transform);
                            
                        }

                        // Delete leftover layers
                        while current_layer_child.is_some() {
                            let trash = current_layer_child.get();
                            do current_layer_child.get().with_common |common| {
                                current_layer_child = common.next_sibling;
                            }
                            root_layer.remove_child(trash);
                        }

                        // Reset zoom
                        *local_zoom = 1f32;
                        root_layer.common.set_transform(identity().translate(-world_offset.x,
                                                                             -world_offset.y,
                                                                             0.0));

                        // TODO: Recycle the old buffers; send them back to the renderer to reuse if
                        // it wishes.

                        *recomposite = true;
                    }
                }
            }
        };

        let profiler_chan = self.profiler_chan.clone();
        let composite = || {
            do profile(time::CompositingCategory, profiler_chan.clone()) {
                debug!("compositor: compositing");
                // Adjust the layer dimensions as necessary to correspond to the size of the window.
                scene.size = window.size();

                // Render the scene.
                rendergl::render_scene(context, scene);
            }

            window.present();
        };

        // When the user scrolls, move the layer around.
        do window.set_scroll_callback |delta| {
            // FIXME (Rust #2528): Can't use `-=`.
            let world_offset_copy = *world_offset;
            *world_offset = world_offset_copy - delta;

            // Clamp the world offset to the screen size.
            let max_x = (page_size.width * *world_zoom - window_size.width as f32).max(&0.0);
            world_offset.x = world_offset.x.clamp(&0.0, &max_x).round();
            let max_y = (page_size.height * *world_zoom - window_size.height as f32).max(&0.0);
            world_offset.y = world_offset.y.clamp(&0.0, &max_y).round();
            
            debug!("compositor: scrolled to %?", *world_offset);
            
            
            let mut scroll_transform = identity();
            
            scroll_transform = scroll_transform.translate(window_size.width as f32 / 2f32 * *local_zoom - world_offset.x,
                                                          window_size.height as f32 / 2f32 * *local_zoom - world_offset.y,
                                                          0.0);
            scroll_transform = scroll_transform.scale(*local_zoom, *local_zoom, 1f32);
            scroll_transform = scroll_transform.translate(window_size.width as f32 / -2f32,
                                                          window_size.height as f32 / -2f32,
                                                          0.0);
            
            root_layer.common.set_transform(scroll_transform);
            
            // FIXME: ask_for_tiles() should be called here, but currently this sends a flood of requests
            // to the renderer, which slows the application dramatically. Instead, ask_for_tiles() is only
            // called on a click event.
//            ask_for_tiles();

            *recomposite = true;
        }



        // When the user pinch-zooms, scale the layer
        do window.set_zoom_callback |magnification| {
            *zoom_action = true;
            *zoom_time = precise_time_s();
            let old_world_zoom = *world_zoom;

            // Determine zoom amount
            *world_zoom = (*world_zoom * magnification).max(&1.0);            
            *local_zoom = *local_zoom * *world_zoom/old_world_zoom;

            // Update world offset
            let corner_to_center_x = world_offset.x + window_size.width as f32 / 2f32;
            let new_corner_to_center_x = corner_to_center_x * *world_zoom / old_world_zoom;
            world_offset.x = world_offset.x + new_corner_to_center_x - corner_to_center_x;

            let corner_to_center_y = world_offset.y + window_size.height as f32 / 2f32;
            let new_corner_to_center_y = corner_to_center_y * *world_zoom / old_world_zoom;
            world_offset.y = world_offset.y + new_corner_to_center_y - corner_to_center_y;        

            // Clamp to page bounds when zooming out
            let max_x = (page_size.width * *world_zoom - window_size.width as f32).max(&0.0);
            world_offset.x = world_offset.x.clamp(&0.0, &max_x).round();
            let max_y = (page_size.height * *world_zoom - window_size.height as f32).max(&0.0);
            world_offset.y = world_offset.y.clamp(&0.0, &max_y).round();
            
            // Apply transformations
            let mut zoom_transform = identity();
            zoom_transform = zoom_transform.translate(window_size.width as f32 / 2f32 * *local_zoom - world_offset.x,
                                                      window_size.height as f32 / 2f32 * *local_zoom - world_offset.y,
                                                      0.0);
            zoom_transform = zoom_transform.scale(*local_zoom, *local_zoom, 1f32);
            zoom_transform = zoom_transform.translate(window_size.width as f32 / -2f32,
                                                      window_size.height as f32 / -2f32,
                                                      0.0);
            root_layer.common.set_transform(zoom_transform);
            
            *recomposite = true;
        }

        // Enter the main event loop.
        while !*done {
            // Check for new messages coming from the rendering task.
            check_for_messages(&self.port);

            // Check for messages coming from the windowing system.
            window.check_loop();

            if *recomposite {
                *recomposite = false;
                composite();
            }

            timer::sleep(&uv_global_loop::get(), 100);

            // If a pinch-zoom happened recently, ask for tiles at the new resolution
            if *zoom_action && precise_time_s() - *zoom_time > 0.3 {
                *zoom_action = false;
                ask_for_tiles();
            }

        }

        self.shutdown_chan.send(())
    }
}

/// A function for spawning into the platform's main thread.
fn on_osmain(f: ~fn()) {
    // FIXME: rust#6399
    let mut main_task = task::task();
    main_task.sched_mode(task::PlatformThread);
    do main_task.spawn {
        f();
    }
}

