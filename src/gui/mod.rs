#![allow(unused_mut)]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]
use neon::prelude::*;
use serde_json::json;
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Mutex, mpsc::{self, Sender, Receiver}},
    thread
};
use winit::{
    dpi::{LogicalSize, LogicalPosition, PhysicalSize, PhysicalPosition, Position},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    event::{Event, WindowEvent, ElementState,  KeyboardInput, VirtualKeyCode, ModifiersState},
    window::{WindowId},
    platform::run_return::EventLoopExtRunReturn
};

use crate::utils::*;
use crate::gpu::runloop;
use crate::context::{BoxedContext2D, page::Page};

pub mod event;
use event::{Cadence, CanvasEvent};

pub mod window;
use window::{Window, WindowSpec};

thread_local!(
    static PROXY: RefCell<Option<EventLoopProxy<CanvasEvent>>> = RefCell::new(None);
);

fn init_proxy(event_loop:&EventLoop<CanvasEvent>){
    PROXY.with(|cell|{
        let mut proxy = cell.borrow_mut();
        *proxy = Some(event_loop.create_proxy());
    });
}

fn send_event(event:CanvasEvent){
    PROXY.with(|cell|{
        if let Some(proxy) = cell.borrow().as_ref(){
            proxy.send_event(event).ok();
        }
    });
}

fn roundtrip<'a, F>(cx: &'a mut FunctionContext, payload:serde_json::Value, callback:&Handle<JsFunction>, mut f:F) -> NeonResult<()>
    where F:FnMut(&str, Page)
{
    let null = cx.null();
    let idents = cx.string(payload.to_string()).upcast::<JsValue>();
    let response = callback.call(cx, null, vec![idents]).expect("Error in Window event handler")
        .downcast::<JsArray, _>(cx).or_throw(cx)?
        .to_vec(cx)?;
    let tokens:Vec<String> = serde_json::from_str(
        &response[0].downcast::<JsString, _>(cx).or_throw(cx)?.value(cx)
    ).unwrap();
    let contexts = response[1].downcast::<JsArray, _>(cx).or_throw(cx)?.to_vec(cx)?
        .iter()
        .map(|obj| obj.downcast::<BoxedContext2D, _>(cx))
        .filter( |ctx| ctx.is_ok() )
        .zip(tokens.iter())
        .for_each(|(ctx, token)| {
            let page = ctx.unwrap().borrow().get_page();
            f(token, page);
        });
    Ok(())
}

pub fn launch(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let mut event_loop: EventLoop<CanvasEvent> = EventLoop::with_user_event();
    init_proxy(&event_loop);

    let win_config = string_arg(&mut cx, 1, "Window configuration")?;
    let contexts = cx.argument::<JsArray>(2)?.to_vec(&mut cx)?;
    let callback = cx.argument::<JsFunction>(3)?;
    contexts.iter()
        .map(|obj| obj.downcast::<BoxedContext2D, _>(&mut cx))
        .zip(serde_json::from_str::<Vec<WindowSpec>>(&win_config).unwrap().iter())
        .filter( |(ctx, _)| ctx.is_ok() )
        .for_each(|(ctx, spec)| {
            let page = ctx.unwrap().borrow().get_page();
            send_event(CanvasEvent::Open(spec.clone(), page));
        });

    let mut frame:u64 = 0;
    let mut offset:LogicalPosition<i32> = LogicalPosition::new(0, 0);
    let mut windows: HashMap<WindowId, Sender<Event<'static, CanvasEvent>>> = HashMap::default();
    let mut window_ids: HashMap<String, WindowId> = HashMap::default();
    let mut cadence = Cadence::new();
    cadence.set_frame_rate(60);


    event_loop.run_return(|event, event_loop, control_flow| {
        runloop(|| {
            match event {
                Event::NewEvents(..) => {
                    *control_flow = cadence.on_next_frame(||{
                        send_event(CanvasEvent::Render)
                    });
                }

                Event::UserEvent(ref canvas_event) => {
                    match canvas_event{
                        CanvasEvent::Open(spec, page) => {
                            let mut spec = spec.clone();
                            spec.x = offset.x;
                            spec.y = offset.y;
                            offset.x += 30;
                            offset.y += 30;
                            let mut window = Window::new(event_loop, &spec, page.clone());
                            let id = window.handle.id();
                            let (tx, rx) = mpsc::channel();

                            window_ids.insert(spec.id.clone(), id);
                            windows.insert(id, tx);

                            thread::spawn(move || {
                                while let Ok(event) = rx.recv() {
                                    window.handle_event(event);
                                }
                            });
                        }
                        CanvasEvent::Close(token) => {
                            if let Some(window_id) = window_ids.get(token){
                                windows.remove(&window_id);
                            }
                        }
                        CanvasEvent::Quit => {
                            return *control_flow = ControlFlow::Exit;
                        }
                        CanvasEvent::Render => {
                            frame += 1;
                            roundtrip(&mut cx, json!({ "frame": frame }), &callback, |token, page| {
                                if let Some(window_id) = window_ids.get(token){
                                    let event = Event::UserEvent(CanvasEvent::Page(page));
                                    if let Some(tx) = windows.get(&window_id) {
                                        if let Some(event) = event.to_static() {
                                            tx.send(event).unwrap();
                                        }
                                    }
                                }
                            }).ok();
                        }
                        _ => {}
                    //   CanvasEvent::Heartbeat => window.autohide_cursor(),
                    //   CanvasEvent::FrameRate(fps) => cadence.set_frame_rate(fps),
                    //   CanvasEvent::InFullscreen(to_full) => window.went_fullscreen(to_full),
                    //   CanvasEvent::Transform(matrix) => window.new_transform(matrix),
                    //   _ => window.send_js_event(canvas_event)
                    }
                  }

                Event::WindowEvent { event:ref win_event, window_id } => match win_event {
                    #[allow(deprecated)]
                    WindowEvent::Destroyed |
                    WindowEvent::CloseRequested |
                    WindowEvent::KeyboardInput { input: KeyboardInput { virtual_keycode: Some(VirtualKeyCode::Escape), state: ElementState::Released,.. }, .. } |
                    WindowEvent::KeyboardInput {
                        input: KeyboardInput {
                            state: ElementState::Released,
                            virtual_keycode: Some(VirtualKeyCode::W),
                            modifiers: ModifiersState::LOGO, ..
                        }, ..
                    } => {
                        windows.remove(&window_id);
                    }
                    _ => {
                        if let Some(tx) = windows.get(&window_id) {
                            if let Some(event) = event.to_static() {
                                tx.send(event).unwrap();
                            }
                        }
                    }
                },

                Event::MainEventsCleared => {
                    *control_flow = match windows.len(){
                        0 => ControlFlow::Exit,
                        _ => ControlFlow::Poll
                    }
                }

                Event::RedrawRequested(window_id) => {
                    if let Some(tx) = windows.get(&window_id) {
                        if let Some(event) = event.to_static() {
                            tx.send(event).unwrap();
                        }
                    }
                }

                _ => {}
            }
        });
    });



    Ok(cx.undefined())
}


pub fn open(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let win_config = string_arg(&mut cx, 0, "Window configuration")?;
    let contexts = cx.argument::<JsArray>(1)?.to_vec(&mut cx)?;
    contexts.iter()
        .map(|obj| obj.downcast::<BoxedContext2D, _>(&mut cx))
        .zip(serde_json::from_str::<Vec<WindowSpec>>(&win_config).unwrap().iter())
        .filter( |(ctx, _)| ctx.is_ok() )
        .for_each(|(ctx, spec)| {
            let page = ctx.unwrap().borrow().get_page();
            send_event(CanvasEvent::Open(spec.clone(), page));
        });

    Ok(cx.undefined())
}

pub fn close(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let token = string_arg(&mut cx, 0, "windowID")?;
    send_event(CanvasEvent::Close(token));
    Ok(cx.undefined())
}

pub fn quit(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    send_event(CanvasEvent::Quit);
    Ok(cx.undefined())
}
