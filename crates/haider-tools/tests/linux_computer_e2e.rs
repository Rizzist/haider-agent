#![cfg(target_os = "linux")]

//! Real Linux computer-use regression anchor.
//!
//! CI runs this ignored test under a 320x240x24 Xvfb server with
//! `HAIDER_CU_E2E=1`. A tiny x11rb client owns a deterministic blue window,
//! paints it red only after a real XTEST click, and reports the actual pointer
//! and keyboard events received from the backend.

use haider_protocol::computer::ComputerAction;
use haider_tools::{
    ComputerBackend, ComputerCancelToken, ComputerOutput, platform_computer_backend,
};
use image::GenericImageView;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, InputFocus, KeyButMask, Rectangle,
    WindowClass,
};
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

const XVFB_WIDTH: u32 = 320;
const XVFB_HEIGHT: u32 = 240;
const MODEL_WIDTH: u32 = 160;
const MODEL_HEIGHT: u32 = 120;
const WINDOW_X: i16 = 20;
const WINDOW_Y: i16 = 20;
const WINDOW_WIDTH: u16 = 200;
const WINDOW_HEIGHT: u16 = 140;
const MODEL_CLICK_X: u32 = 30;
const MODEL_CLICK_Y: u32 = 30;
const ROOT_CLICK_X: i16 = 60;
const ROOT_CLICK_Y: i16 = 60;
const XK_CONTROL_L: u32 = 0xffe3;

#[derive(Debug)]
enum ClientEvent {
    Ready,
    Button {
        pressed: bool,
        button: u8,
        root_x: i16,
        root_y: i16,
    },
    Key {
        keysym: u32,
        state: KeyButMask,
    },
    Failed(String),
}

struct FixtureKeymap {
    minimum: u8,
    maximum: u8,
    levels: usize,
    keysyms: Vec<u32>,
}

impl FixtureKeymap {
    fn keysym(&self, keycode: u8, shifted: bool) -> u32 {
        if keycode < self.minimum || keycode > self.maximum || self.levels == 0 {
            return 0;
        }
        let base = usize::from(keycode - self.minimum) * self.levels;
        let level = usize::from(shifted && self.levels > 1);
        self.keysyms.get(base + level).copied().unwrap_or(0)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires HAIDER_CU_E2E=1 and a 320x240x24 Xvfb display"]
async fn xvfb_real_pixels_pointer_click_and_keyboard_round_trip() {
    assert_eq!(
        std::env::var("HAIDER_CU_E2E").as_deref(),
        Ok("1"),
        "the ignored Linux computer e2e must only run when HAIDER_CU_E2E=1"
    );
    let display = match std::env::var("DISPLAY") {
        Ok(display) => display,
        Err(error) => panic!("Xvfb must provide DISPLAY: {error}"),
    };
    assert!(!display.is_empty(), "Xvfb DISPLAY must not be empty");

    let (sender, receiver) = mpsc::channel();
    let failure_sender = sender.clone();
    let client = std::thread::spawn(move || {
        let result = run_fixture_client(&display, sender);
        if let Err(error) = &result {
            let _ = failure_sender.send(ClientEvent::Failed(error.clone()));
        }
        result
    });
    expect_ready(&receiver);

    let backend = platform_computer_backend();
    let cancel = ComputerCancelToken::new();
    let before = screenshot(&*backend, &cancel).await;
    assert_eq!(before.dimensions(), (XVFB_WIDTH, XVFB_HEIGHT));
    assert_eq!(
        before.get_pixel(30, 30).0,
        [0, 0, 255, 255],
        "fixture window must begin blue"
    );
    backend
        .set_viewport(MODEL_WIDTH, MODEL_HEIGHT)
        .unwrap_or_else(|error| panic!("downscaled CU-1 viewport must install: {error}"));

    expect_confirmed(
        backend
            .execute(
                &ComputerAction::LeftClick {
                    x: MODEL_CLICK_X,
                    y: MODEL_CLICK_Y,
                },
                &cancel,
            )
            .await,
        "left_click",
    );
    for expected_pressed in [true, false] {
        match recv_event(&receiver, "button press/release") {
            ClientEvent::Button {
                pressed,
                button,
                root_x,
                root_y,
            } => {
                assert_eq!(pressed, expected_pressed);
                assert_eq!(button, 1, "left_click must emit X11 button 1");
                assert_eq!((root_x, root_y), (ROOT_CLICK_X, ROOT_CLICK_Y));
            }
            event => panic!("expected fixture button event, got {event:?}"),
        }
    }

    let after = screenshot(&*backend, &cancel).await;
    assert_eq!(after.dimensions(), (XVFB_WIDTH, XVFB_HEIGHT));
    assert_eq!(
        after.get_pixel(30, 30).0,
        [255, 0, 0, 255],
        "real XTEST click must make the fixture repaint red before recapture"
    );
    backend
        .set_viewport(MODEL_WIDTH, MODEL_HEIGHT)
        .unwrap_or_else(|error| panic!("second screenshot viewport must install: {error}"));
    let cursor = backend
        .execute(&ComputerAction::CursorPosition, &cancel)
        .await
        .unwrap_or_else(|error| panic!("QueryPointer must observe XTEST motion: {error}"));
    match cursor {
        ComputerOutput::CursorPosition { x, y } => {
            assert_eq!((x, y), (MODEL_CLICK_X, MODEL_CLICK_Y));
        }
        output => panic!("expected cursor position, got {output:?}"),
    }

    expect_confirmed(
        backend
            .execute(&ComputerAction::Type { text: "Az!".into() }, &cancel)
            .await,
        "type",
    );
    let uppercase = recv_key(&receiver, u32::from(b'A'));
    assert!(
        uppercase.contains(KeyButMask::SHIFT),
        "A must arrive with Shift"
    );
    let lowercase = recv_key(&receiver, u32::from(b'z'));
    assert!(
        !lowercase.contains(KeyButMask::SHIFT),
        "z must arrive without Shift"
    );
    let bang = recv_key(&receiver, u32::from(b'!'));
    assert!(bang.contains(KeyButMask::SHIFT), "! must arrive with Shift");

    expect_confirmed(
        backend
            .execute(
                &ComputerAction::Key {
                    keys: "ctrl+a".into(),
                },
                &cancel,
            )
            .await,
        "key",
    );
    let control_a = recv_key(&receiver, u32::from(b'a'));
    assert!(
        control_a.contains(KeyButMask::CONTROL),
        "ctrl+a main key must arrive with Control held"
    );

    match client.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("fixture X11 client failed: {error}"),
        Err(_) => panic!("fixture X11 client panicked"),
    }
}

async fn screenshot(
    backend: &dyn ComputerBackend,
    cancel: &ComputerCancelToken,
) -> image::DynamicImage {
    let output = backend
        .execute(&ComputerAction::Screenshot, cancel)
        .await
        .unwrap_or_else(|error| panic!("real X11 screenshot must succeed: {error}"));
    let ComputerOutput::ScreenshotPng(png) = output else {
        panic!("screenshot action returned {output:?}");
    };
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(png.len() > 100, "encoded PNG must be non-empty");
    image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .unwrap_or_else(|error| panic!("backend PNG must decode: {error}"))
}

fn expect_confirmed(result: haider_tools::ComputerResult<ComputerOutput>, expected_action: &str) {
    let output = result.unwrap_or_else(|error| panic!("real XTEST action must succeed: {error}"));
    match output {
        ComputerOutput::Confirmed { action } => assert_eq!(action, expected_action),
        output => panic!("expected confirmed {expected_action}, got {output:?}"),
    }
}

fn expect_ready(receiver: &Receiver<ClientEvent>) {
    match recv_event(receiver, "fixture readiness") {
        ClientEvent::Ready => {}
        event => panic!("expected ready fixture, got {event:?}"),
    }
}

fn recv_key(receiver: &Receiver<ClientEvent>, expected: u32) -> KeyButMask {
    loop {
        match recv_event(receiver, "key event") {
            ClientEvent::Key { keysym, state } if keysym == expected => return state,
            ClientEvent::Key { .. } => {}
            event => panic!("expected keysym 0x{expected:x}, got {event:?}"),
        }
    }
}

fn recv_event(receiver: &Receiver<ClientEvent>, purpose: &str) -> ClientEvent {
    match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(ClientEvent::Failed(error)) => {
            panic!("fixture failed while waiting for {purpose}: {error}")
        }
        Ok(event) => event,
        Err(error) => panic!("timed out waiting for {purpose}: {error}"),
    }
}

fn run_fixture_client(display_name: &str, sender: Sender<ClientEvent>) -> Result<(), String> {
    let (connection, screen_number) =
        x11rb::connect(Some(display_name)).map_err(|error| error.to_string())?;
    let screen = connection
        .setup()
        .roots
        .get(screen_number)
        .ok_or_else(|| "Xvfb did not expose its selected screen".to_owned())?;
    let root = screen.root;
    let window = connection
        .generate_id()
        .map_err(|error| error.to_string())?;
    connection
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            WINDOW_X,
            WINDOW_Y,
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(0x0000ff)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::KEY_PRESS
                        | EventMask::KEY_RELEASE,
                ),
        )
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    connection
        .map_window(window)
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    connection
        .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    let gc = connection
        .generate_id()
        .map_err(|error| error.to_string())?;
    connection
        .create_gc(gc, window, &CreateGCAux::new().foreground(0xff0000))
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    connection.flush().map_err(|error| error.to_string())?;

    let setup = connection.setup();
    let count = setup
        .max_keycode
        .checked_sub(setup.min_keycode)
        .and_then(|difference| difference.checked_add(1))
        .ok_or_else(|| "Xvfb returned an invalid keycode range".to_owned())?;
    let mapping = connection
        .get_keyboard_mapping(setup.min_keycode, count)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
    let keymap = FixtureKeymap {
        minimum: setup.min_keycode,
        maximum: setup.max_keycode,
        levels: usize::from(mapping.keysyms_per_keycode),
        keysyms: mapping.keysyms,
    };
    sender
        .send(ClientEvent::Ready)
        .map_err(|_| "test receiver closed before fixture readiness".to_owned())?;

    let mut saw_control_a = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(
                "fixture timed out before receiving the complete click/type/key event sequence"
                    .into(),
            );
        }
        let Some(event) = connection
            .poll_for_event()
            .map_err(|error| error.to_string())?
        else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        match event {
            Event::ButtonPress(button) => {
                connection
                    .poly_fill_rectangle(
                        window,
                        gc,
                        &[Rectangle {
                            x: 0,
                            y: 0,
                            width: WINDOW_WIDTH,
                            height: WINDOW_HEIGHT,
                        }],
                    )
                    .map_err(|error| error.to_string())?
                    .check()
                    .map_err(|error| error.to_string())?;
                sender
                    .send(ClientEvent::Button {
                        pressed: true,
                        button: button.detail,
                        root_x: button.root_x,
                        root_y: button.root_y,
                    })
                    .map_err(|_| "test receiver closed during button press".to_owned())?;
            }
            Event::ButtonRelease(button) => {
                sender
                    .send(ClientEvent::Button {
                        pressed: false,
                        button: button.detail,
                        root_x: button.root_x,
                        root_y: button.root_y,
                    })
                    .map_err(|_| "test receiver closed during button release".to_owned())?;
            }
            Event::KeyPress(key) => {
                let keysym = keymap.keysym(key.detail, key.state.contains(KeyButMask::SHIFT));
                if keysym == u32::from(b'a') && key.state.contains(KeyButMask::CONTROL) {
                    saw_control_a = true;
                }
                sender
                    .send(ClientEvent::Key {
                        keysym,
                        state: key.state,
                    })
                    .map_err(|_| "test receiver closed during key press".to_owned())?;
            }
            Event::KeyRelease(key)
                if saw_control_a && keymap.keysym(key.detail, false) == XK_CONTROL_L =>
            {
                break;
            }
            _ => {}
        }
    }

    connection
        .free_gc(gc)
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    connection
        .destroy_window(window)
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    Ok(())
}
