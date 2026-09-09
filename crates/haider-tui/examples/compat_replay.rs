//! Replay a caller-supplied, sanitized journal through the shipping live loop.
//! No daemon, provider or credential storage is opened by this probe.
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::SessionId;
use haider_rpc::AttachmentId;
use haider_tui::app::{AppModel, RuntimeMode};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::runtime::live_pass;
use std::io::BufRead;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: compat_replay SANITIZED_JSONL")?;
    let rows: Vec<RawEnvelope> = std::io::BufReader::new(std::fs::File::open(path)?)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect::<Result<_, Box<dyn std::error::Error>>>()?;
    let head = rows.last().ok_or("empty journal")?.seq;
    let session = SessionId::new("compat-session");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    let mut driver = LiveDriver::new("journal-replay");
    driver.apply(
        &mut model,
        LiveReply::Handshake {
            features: Default::default(),
            version: env!("CARGO_PKG_VERSION").into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            protocol: haider_rpc::WIRE_PROTOCOL_VERSION,
        },
    );
    driver.apply(
        &mut model,
        LiveReply::Listed {
            sessions: vec![serde_json::from_value(serde_json::json!({
                "session_id":session,"head_seq":head,"worker_generation":1,
            }))?],
            next_cursor: None,
        },
    );
    model.open_session(&session);
    driver.sync_selection(&model);
    let mut attachment = AttachmentId::new("initial");
    attach(&mut driver, &mut model, &session, &attachment, head);
    model.requests.clear();
    let gap_at = head / 3;
    let lag_at = 2 * head / 3;
    let mut gapped = false;
    let mut lagged = false;
    let mut index = 0;
    let mut recoveries = Vec::new();
    while index < rows.len() {
        let row = &rows[index];
        if !gapped && row.seq == gap_at {
            gapped = true;
            index += 1; // force actual missing delivery, not a synthetic flag
            continue;
        }
        let reply = if !lagged && row.seq == lag_at {
            lagged = true;
            LiveReply::Lagged {
                attachment: attachment.clone(),
            }
        } else {
            let mut envelope = row.clone();
            envelope.session_id = session.clone();
            index += 1;
            LiveReply::Event {
                attachment: attachment.clone(),
                session: session.clone(),
                envelope: Box::new(envelope),
            }
        };
        let pass = live_pass(
            &mut driver,
            &mut model,
            Some(reply),
            std::time::Instant::now(),
        );
        if model.compatibility_diagnostic.is_some() || !model.stream_diagnostics.is_empty() {
            return Err(format!("unexpected diagnostic at {}", row.seq).into());
        }
        for command in pass.commands {
            match command {
                LiveCommand::SessionDiagnostic { .. } => return Err("false durable latch".into()),
                LiveCommand::Attach { after_seq, .. } => {
                    if !model.resyncing.contains(&session) {
                        return Err("resync was invisible".into());
                    }
                    recoveries.push(after_seq);
                    if recoveries.len() > 2 {
                        return Err("extra recovery".into());
                    }
                    attachment = AttachmentId::new(format!("replay-{}", recoveries.len()));
                    attach(&mut driver, &mut model, &session, &attachment, head);
                    index = rows.partition_point(|row| row.seq <= after_seq);
                }
                _ => {}
            }
        }
    }
    driver.apply(
        &mut model,
        LiveReply::CaughtUp {
            attachment,
            high_water_seq: head,
        },
    );
    if model.projection.last_applied() != Some(head)
        || !model.resyncing.is_empty()
        || model.projection.unknown_payloads() != 0
        || recoveries.len() != 2
    {
        return Err("replay did not settle at the complete journal cursor".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "events":rows.len(),"last_applied":model.projection.last_applied(),
            "forced_gap_at":gap_at,"forced_lagged_at":lag_at,"replay_after_cursors":recoveries,
            "unknown_payloads":model.projection.unknown_payloads(),"incompatible":false,"resyncing":false,
        })
    );
    Ok(())
}

fn attach(
    driver: &mut LiveDriver,
    model: &mut AppModel,
    session: &SessionId,
    attachment: &AttachmentId,
    head: u64,
) {
    driver.apply(
        model,
        LiveReply::Attached {
            session: session.clone(),
            attachment: attachment.clone(),
            worker_generation: 1,
            replay_through_seq: head,
        },
    );
}
