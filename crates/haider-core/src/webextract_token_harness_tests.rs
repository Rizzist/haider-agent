#![allow(clippy::expect_used)]

use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::model_tool_result_projection;

const BLOATED_STATIC_TEMPLATE: &str =
    include_str!("../tests/fixtures/webextract-tokens/bloated-static.html.tmpl");
const JS_SHELL_TEMPLATE: &str =
    include_str!("../tests/fixtures/webextract-tokens/js-shell.html.tmpl");
const TABLE_HEAVY_TEMPLATE: &str =
    include_str!("../tests/fixtures/webextract-tokens/table-heavy.html.tmpl");
const JSON_API_TEMPLATE: &str =
    include_str!("../tests/fixtures/webextract-tokens/api-response.json.tmpl");

#[derive(Clone)]
struct Fixture {
    name: &'static str,
    path: &'static str,
    content_type: &'static str,
    body: String,
}

#[derive(Serialize)]
struct MeasurementReport {
    schema: &'static str,
    arm: String,
    estimator: &'static str,
    fetch_output_cap_bytes: usize,
    untruncated_model_cap_bytes: usize,
    rows: Vec<MeasurementRow>,
}

#[derive(Serialize)]
struct MeasurementRow {
    fixture: &'static str,
    path: &'static str,
    content_type: &'static str,
    source_bytes: usize,
    source_tokens: usize,
    uncapped_passthrough_bytes: usize,
    uncapped_passthrough_tokens: usize,
    fetch_payload_bytes: usize,
    fetch_tokens: usize,
    model_projection_bytes: usize,
    model_tokens: usize,
    fetch_truncated: bool,
    model_truncated: bool,
    js_shell_marker: bool,
    table_preserved: bool,
}

fn fixtures() -> Vec<Fixture> {
    let main_rows = (1..=36)
        .map(|index| {
            format!(
                "<p>Section {index} explains deterministic extraction, bounded retrieval, and the practical tradeoffs of keeping evidence concise while preserving facts, links, and code for an agent.</p>"
            )
        })
        .collect::<String>();
    let boilerplate_rows = (1..=72)
        .map(|index| {
            format!(
                "<section class=\"promo-card\"><h2>Recommended item {index}</h2><p>Promotional navigation, repeated account controls, related links, tracking labels, and newsletter copy are page chrome rather than article evidence.</p></section>"
            )
        })
        .collect::<String>();
    let script_bloat = (1..=1_600)
        .map(|index| {
            format!(
                "window.__fixture.push({{id:{index},route:'/tracking/{index}',label:'hydration payload {index}'}});"
            )
        })
        .collect::<String>();
    let bloated = BLOATED_STATIC_TEMPLATE
        .replace("{{MAIN_ROWS}}", &main_rows)
        .replace("{{BOILERPLATE_ROWS}}", &boilerplate_rows)
        .replace("{{SCRIPT_BLOAT}}", &script_bloat);

    let js_payload = (1..=900)
        .map(|index| {
            format!("window.__routeChunks.push({{chunk:{index},module:'dashboard-pane-{index}'}});")
        })
        .collect::<String>();
    let js_shell = JS_SHELL_TEMPLATE.replace("{{SCRIPT_BLOAT}}", &js_payload);

    let table_rows = (1..=320)
        .map(|index| {
            format!(
                "<tr><td>pkg-{index:03}</td><td>{}</td><td>{}</td><td>Owner {}</td><td><code>sha256:{index:064x}</code></td></tr>",
                if index % 3 == 0 { "degraded" } else { "healthy" },
                20 + index % 80,
                index % 17,
            )
        })
        .collect::<String>();
    let table_heavy = TABLE_HEAVY_TEMPLATE.replace("{{TABLE_ROWS}}", &table_rows);

    let json_rows = (1..=360)
        .map(|index| {
            format!(
                "    {{\n      \"id\": {index},\n      \"name\": \"artifact-{index:03}\",\n      \"status\": \"{}\",\n      \"owner\": \"team-{}\",\n      \"description\": \"escaped \\\"quoted\\\" diagnostic for artifact {index} at C:\\\\fixtures\\\\artifact-{index:03}\",\n      \"labels\": [\"web\", \"measurement\", \"batch-{}\"]\n    }}",
                if index % 5 == 0 { "queued" } else { "ready" },
                index % 13,
                index % 9,
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let json_api = JSON_API_TEMPLATE.replace("{{JSON_ROWS}}", &json_rows);

    vec![
        Fixture {
            name: "bloated_static",
            path: "/bloated-static",
            content_type: "text/html; charset=utf-8",
            body: bloated,
        },
        Fixture {
            name: "js_shell",
            path: "/js-shell",
            content_type: "text/html; charset=utf-8",
            body: js_shell,
        },
        Fixture {
            name: "table_heavy",
            path: "/table-heavy",
            content_type: "text/html; charset=utf-8",
            body: table_heavy,
        },
        Fixture {
            name: "json_api",
            path: "/api/items",
            content_type: "application/json",
            body: json_api,
        },
    ]
}

async fn spawn_fixture_server(fixtures: &[Fixture]) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind token fixture server");
    let base = format!("http://{}", listener.local_addr().expect("fixture address"));
    let routes = fixtures
        .iter()
        .map(|fixture| {
            (
                fixture.path.to_owned(),
                (fixture.content_type.to_owned(), fixture.body.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut request = vec![0_u8; 8 * 1024];
                let mut used = 0_usize;
                loop {
                    let Ok(read) = socket.read(&mut request[used..]).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    used += read;
                    if request[..used]
                        .windows(4)
                        .any(|window| window == b"\r\n\r\n")
                    {
                        break;
                    }
                    if used == request.len() {
                        return;
                    }
                }
                let request = String::from_utf8_lossy(&request[..used]);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let response = routes.get(path).map_or_else(
                    || b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
                    |(content_type, body)| {
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(body.as_bytes());
                        response
                    },
                );
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (base, server)
}

fn bounded_fetch_result(
    final_url: &str,
    content_type: &str,
    outcome: &haider_provider::WebFetchOutcome,
) -> BoundedResult {
    let mut result = BoundedResult {
        preview: format!("[{final_url} · {content_type}]\n{}", outcome.text),
        truncated: outcome.truncated,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    };
    if let Some(truncation) = outcome.truncation.clone() {
        result.declare_truncation(truncation);
    }
    result
}

#[test]
fn webextract_token_fixture_set_is_deterministic_and_meaningful() {
    let fixtures = fixtures();
    assert_eq!(fixtures.len(), 4);
    let bloated = &fixtures[0].body;
    assert!(
        bloated.len() > 96 * 1024,
        "bloated source reaches the output-cap arm"
    );
    assert!(bloated.contains("Section 36 explains deterministic extraction"));
    assert!(bloated.contains("Recommended item 72"));

    let shell = &fixtures[1].body;
    assert!(shell.len() > 48 * 1024);
    assert!(shell.contains("<div id=\"app\"></div>"));
    assert!(!shell.contains("haider_js_shell"));

    let table = &fixtures[2].body;
    assert!(table.contains("pkg-320"));
    assert!(table.contains("<table>"));

    let json = &fixtures[3].body;
    let parsed: serde_json::Value = serde_json::from_str(json).expect("expanded JSON fixture");
    assert_eq!(parsed["items"].as_array().map(Vec::len), Some(360));
}

/// Measurement harness, deliberately ignored by ordinary suites. It serves
/// the committed templates over loopback, traverses the production guarded
/// fetcher, then applies the exact actor-owned model projection and landed
/// serialized-byte estimator. Comparative arms are built in disposable
/// worktrees; `HAIDER_WEBEXTRACT_TOKEN_ARM` records which source was run.
#[tokio::test]
#[ignore = "explicit token-economics measurement harness"]
async fn measure_webextract_token_economics() {
    let arm = env::var("HAIDER_WEBEXTRACT_TOKEN_ARM").unwrap_or_else(|_| "current".to_owned());
    assert!(
        matches!(arm.as_str(), "current" | "legacy" | "passthrough"),
        "unknown measurement arm {arm:?}"
    );
    let fixtures = fixtures();
    let (base, server) = spawn_fixture_server(&fixtures).await;
    let mut rows = Vec::new();
    for fixture in &fixtures {
        let outcome = haider_provider::fetch_public_url(&format!("{base}{}", fixture.path), None)
            .await
            .expect("fixture fetch succeeds");
        let result = bounded_fetch_result(&outcome.final_url, &outcome.content_type, &outcome);
        let projection = model_tool_result_projection("web_fetch", &result);
        let uncapped_passthrough = format!(
            "[{} · {}]\n{}",
            outcome.final_url, outcome.content_type, fixture.body
        );
        rows.push(MeasurementRow {
            fixture: fixture.name,
            path: fixture.path,
            content_type: fixture.content_type,
            source_bytes: fixture.body.len(),
            source_tokens: haider_tools::estimated_text_tokens(&fixture.body),
            uncapped_passthrough_bytes: uncapped_passthrough.len(),
            uncapped_passthrough_tokens: haider_tools::estimated_text_tokens(&uncapped_passthrough),
            fetch_payload_bytes: result.payload_text().len(),
            fetch_tokens: haider_tools::estimated_text_tokens(result.payload_text()),
            model_projection_bytes: projection.preview.len(),
            model_tokens: haider_tools::estimated_text_tokens(&projection.preview),
            fetch_truncated: outcome.truncated,
            model_truncated: projection.truncated,
            js_shell_marker: outcome.text.contains("haider_js_shell")
                || outcome.text.contains("js-shell"),
            table_preserved: outcome
                .text
                .contains("| Package | Status | Latency | Owner | Digest |")
                && outcome.text.contains("| --- | --- | --- | --- | --- |"),
        });
    }
    server.abort();
    let report = MeasurementReport {
        schema: "haider.webextract-token-measurement.v1",
        arm,
        estimator: "ceil(serialized provider text JSON-string bytes / 4)",
        fetch_output_cap_bytes: haider_provider::WEB_FETCH_OUTPUT_CAP_BYTES,
        untruncated_model_cap_bytes: 16 * 1024,
        rows,
    };
    let rendered = serde_json::to_string_pretty(&report).expect("serialize measurement report");
    if let Some(path) = env::var_os("HAIDER_WEBEXTRACT_TOKEN_OUTPUT") {
        std::fs::write(Path::new(&path), format!("{rendered}\n"))
            .expect("write requested measurement report");
    }
    eprintln!("{rendered}");
}
