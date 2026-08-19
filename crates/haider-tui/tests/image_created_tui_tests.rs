//! Durable image-created rows: styled/plain parity, click identity, and the
//! platform reveal command boundary.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::image::{IMAGE_CREATED_EXTENSION_KIND, ImageCreatedV1};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit};
use haider_tui::browser::reveal_path_command;
use haider_tui::live::LiveDriver;
use haider_tui::mock::demo_script;
use haider_tui::plain::render_plain;
use haider_tui::render::render;
use haider_tui::runtime::{ShellRequest, live_pass};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn image_model() -> (AppModel, ImageCreatedV1) {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    let image = ImageCreatedV1 {
        path: "/workspace/artifacts/chart.png".into(),
        display_path: "artifacts/chart.png".into(),
        media_type: "image/png".into(),
        byte_len: 2_049,
        width: Some(640),
        height: Some(360),
        call_id: "call-image".into(),
        tool: "process_exec".into(),
    };
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("image-created-test"),
            item: TurnItem::Extension {
                kind: IMAGE_CREATED_EXTENSION_KIND.into(),
                data: serde_json::to_value(&image).expect("image payload serializes"),
            },
        }));
    (model, image)
}

/// MUTATION CHECK: route image extensions through the generic extension row,
/// drop dimensions/size, or omit the reveal hit. Expected runtime failure:
/// the transcript and value-carrying hit assertions below.
#[test]
fn image_extension_renders_and_registers_its_absolute_reveal_path() {
    let (model, image) = image_model();
    let backend = TestBackend::new(118, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(&model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| row.contains("🖼 image · artifacts/chart.png · 640×360 · 3 KB")),
        "typed image row missing:\n{}",
        rows.join("\n")
    );
    assert!(
        hits.iter()
            .any(|(_, hit)| hit == &Hit::RevealPath(image.path.clone())),
        "the row hit carries the durable absolute path"
    );

    let plain = render_plain(&model.projection, 0, None);
    assert!(
        plain.contains("🖼 image · artifacts/chart.png · 640×360 · 3 KB"),
        "plain mode must project the same payload: {plain}"
    );
}

/// MUTATION CHECK: perform IO in the reducer or lose the clicked path while
/// crossing to the shell. Expected runtime failure: the exact request pin.
#[test]
fn image_row_click_requests_a_shell_reveal_by_value() {
    let (mut model, image) = image_model();
    model.requests.clear();
    model.handle_hit(Hit::RevealPath(image.path.clone()));
    assert_eq!(
        model.requests,
        vec![AppRequest::RevealPath {
            path: image.path.clone()
        }]
    );
    let mut driver = LiveDriver::new("test");
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    assert_eq!(pass.shell, vec![ShellRequest::RevealPath(image.path)]);
}

/// MUTATION CHECK: remove the macOS `-R`, Windows `/select,`, or Linux parent
/// directory behavior. Expected runtime failure on the corresponding host.
#[test]
fn reveal_command_selects_the_file_on_this_platform() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("created.png");
    std::fs::write(&file, b"png").expect("test file");
    let command = reveal_path_command(&file).expect("existing paths are revealable");
    // Verify round 2: the trust boundary canonicalizes before spawning, so
    // the platform arg is the CANONICAL form (tempdir symlinks resolved).
    let file = file.canonicalize().expect("canonical");
    #[allow(unused_variables)]
    let dir_canonical = dir.path().canonicalize().expect("canonical dir");
    let program = command.get_program().to_string_lossy();
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    if cfg!(target_os = "macos") {
        assert_eq!(program, "/usr/bin/open");
        assert_eq!(
            args,
            vec!["-R".to_owned(), file.to_string_lossy().into_owned()]
        );
    } else if cfg!(target_os = "windows") {
        assert_eq!(program, "explorer");
        assert_eq!(args, vec![format!("/select,{}", file.display())]);
    } else {
        assert_eq!(program, "xdg-open");
        assert_eq!(args, vec![dir_canonical.to_string_lossy().into_owned()]);
    }

    assert!(
        reveal_path_command(&dir.path().join("missing.png")).is_err(),
        "non-existent payloads are refused before spawn"
    );
}

/// Verify round 2 MUTATION CHECK: drop any reveal trust-boundary check
/// (absolute, image extension, regular file). Expected RUNTIME failure: an
/// untrusted durable path reaches the OS opener.
#[test]
fn reveal_refuses_untrusted_shapes() {
    use haider_tui::browser::reveal_path_command;
    use std::path::Path;
    // Relative (option-shaped) paths refuse outright.
    assert!(reveal_path_command(Path::new("-e")).is_err());
    assert!(reveal_path_command(Path::new("--help/x.png")).is_err());
    // Absolute non-image refuses even when it exists.
    assert!(reveal_path_command(Path::new("/etc/hosts")).is_err());
    // A directory with an image-ish name refuses.
    let dir = tempfile::tempdir().expect("dir");
    let fake = dir.path().join("folder.png");
    std::fs::create_dir(&fake).expect("mkdir");
    assert!(reveal_path_command(&fake).is_err());
    // A real absolute image file passes.
    let good = dir.path().join("real.png");
    std::fs::write(&good, [0x89, b'P', b'N', b'G']).expect("write");
    assert!(reveal_path_command(&good).is_ok());
}

/// Round 3 MUTATION CHECK: check the extension only BEFORE canonicalization.
/// Expected RUNTIME failure: a symlink named cover.png targeting a non-image
/// reaches the OS opener.
#[cfg(unix)]
#[test]
fn reveal_refuses_image_named_symlinks_to_non_images() {
    use haider_tui::browser::reveal_path_command;
    let dir = tempfile::tempdir().expect("dir");
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, b"not an image").expect("write");
    let cover = dir.path().join("cover.png");
    std::os::unix::fs::symlink(&secret, &cover).expect("symlink");
    assert!(
        reveal_path_command(&cover).is_err(),
        "an image-named symlink to a non-image must refuse"
    );
    // A symlink to a REAL image stays revealable (canonical target passes).
    let real = dir.path().join("real.jpg");
    std::fs::write(&real, [0xFF, 0xD8, 0xFF]).expect("write");
    let alias = dir.path().join("alias.jpeg");
    std::os::unix::fs::symlink(&real, &alias).expect("symlink");
    assert!(reveal_path_command(&alias).is_ok());
}
