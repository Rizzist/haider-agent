#![deny(unsafe_op_in_unsafe_fn)]

use haider_daemon::{DaemonConfig, DaemonDependencies, DaemonState, ShutdownOutcome};
use haider_platform::Endpoint;
use jni::JNIEnv;
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;

const OK: jint = 0;
const ALREADY_RUNNING: jint = 1;
const NOT_INITIALIZED: jint = 2;
const BAD_PATHS: jint = 3;
const BAD_POLICY: jint = 4;
const VAULT_KEY_INVALID: jint = 5;
const INTERNAL: jint = 7;
const TIMEOUT: jint = 8;
// DRAFT C1 omits numeric shutdown outcomes. Spike: Graceful=0, Forced=9.
const FORCED: jint = 9;
const FRAME_LIMIT: usize = 8 * 1024 * 1024;
static STATE: OnceLock<Mutex<Option<EmbeddedDaemon>>> = OnceLock::new();

type Join =
    Pin<Box<dyn Future<Output = Result<ShutdownOutcome, haider_daemon::DaemonError>> + Send>>;

struct Running {
    // Drop the runtime BEFORE releasing the global context: all worker and
    // blocking JNI readers must have exited before ndk-context is cleared.
    runtime: Runtime,
    join: Join,
    readiness: haider_daemon::Readiness,
    diagnostics: haider_daemon::DaemonTaskDiagnostics,
    shutdown: haider_daemon::ShutdownHandle,
    generation: Option<u64>,
}

struct EmbeddedDaemon {
    _context: GlobalRef,
    config: DaemonConfig,
    running: Option<Running>,
    last: Value,
}

impl Drop for EmbeddedDaemon {
    fn drop(&mut self) {
        // Also preserves context lifetime if a JNI operation unwinds after
        // taking the owner out of STATE. Runtime drop joins blocking readers.
        drop(self.running.take());
        // SAFETY: this owner initialized ndk-context exactly once. Its runtime
        // readers are gone, STATE serializes owners, and _context is still live.
        unsafe { ndk_context::release_android_context() };
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Paths {
    profile_id: String,
    store_dir: PathBuf,
    runtime_dir: PathBuf,
    logs_dir: PathBuf,
    workspace_dir: PathBuf,
    tmp_dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpikePolicy {
    spike_use_default_dependencies: bool,
    default_model: Option<String>,
}

fn state() -> &'static Mutex<Option<EmbeddedDaemon>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn failed(code: &str) -> Value {
    json!({"phase":"Failed", "daemon_generation":null, "error_code":code, "retryable":false})
}

/// Catch both Rust errors and panics, and consume any exception raised by our
/// JNI calls. Failure strings never contain paths, Java errors, or daemon data.
fn boundary<T>(
    env: &mut JNIEnv<'_>,
    fallback: T,
    operation: impl FnOnce(&mut JNIEnv<'_>) -> T,
) -> T {
    let result = catch_unwind(AssertUnwindSafe(|| operation(env)));
    let exception = env.exception_check().unwrap_or(true);
    if exception {
        let _ = env.exception_clear();
    }
    match result {
        Ok(value) if !exception => value,
        _ => {
            let mut slot = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(daemon) = slot.as_mut() {
                daemon.last = failed("INTERNAL");
            }
            fallback
        }
    }
}

fn java_json(env: &mut JNIEnv<'_>, value: Value) -> jstring {
    env.new_string(value.to_string())
        .map(JString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

fn log_status(env: &mut JNIEnv<'_>, operation: &str, status: jint) -> jint {
    // Only exception inspection/clear is permitted while Java is throwing.
    // Leave it for boundary() to clear and map to INTERNAL.
    if env.exception_check().unwrap_or(true) {
        return status;
    }
    let Ok(tag) = env.new_string("HaiderNative") else {
        return status;
    };
    let Ok(message) = env.new_string(format!("{operation} status={status}")) else {
        return status;
    };
    let _ = env.call_static_method(
        "android/util/Log",
        "i",
        "(Ljava/lang/String;Ljava/lang/String;)I",
        &[JValue::Object(&tag), JValue::Object(&message)],
    );
    status
}

// SAFETY: these symbols and typed JNI arguments match NativeDaemon.kt exactly.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeVersion(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    boundary(&mut env, std::ptr::null_mut(), |env| {
        java_json(
            env,
            json!({
                "daemon_version":env!("CARGO_PKG_VERSION"),
                "wire_protocol":haider_rpc::WIRE_PROTOCOL_VERSION,
                "build_id":option_env!("HAIDER_ANDROID_BUILD_ID").unwrap_or("971-1-spike-unidentified"),
                "abi":if cfg!(target_arch = "aarch64") { "arm64-v8a" } else { "x86_64" }, "jni_version":1
            }),
        )
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeInit(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    context: JObject<'_>,
    paths: JString<'_>,
) -> jint {
    boundary(&mut env, INTERNAL, |env| {
        let status = init(env, context, paths).unwrap_or_else(|code| code);
        log_status(env, "nativeInit", status)
    })
}

fn init(env: &mut JNIEnv<'_>, context: JObject<'_>, paths: JString<'_>) -> Result<jint, jint> {
    let text: String = env.get_string(&paths).map_err(|_| BAD_PATHS)?.into();
    let paths: Paths = serde_json::from_str(&text).map_err(|_| BAD_PATHS)?;
    let app = env
        .call_method(
            context,
            "getApplicationContext",
            "()Landroid/content/Context;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(|_| INTERNAL)?;
    let files = env
        .call_method(&app, "getFilesDir", "()Ljava/io/File;", &[])
        .and_then(|v| v.l())
        .map_err(|_| INTERNAL)?;
    let root = env
        .call_method(files, "getCanonicalPath", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(|_| INTERNAL)?;
    let root: String = env
        .get_string(&JString::from(root))
        .map_err(|_| INTERNAL)?
        .into();
    let root = PathBuf::from(root);
    // Host creates these directories. Canonical containment rejects traversal
    // and symlink escape without touching any unvalidated directory.
    for path in [
        &paths.store_dir,
        &paths.runtime_dir,
        &paths.logs_dir,
        &paths.workspace_dir,
        &paths.tmp_dir,
    ] {
        let canonical = path.canonicalize().map_err(|_| BAD_PATHS)?;
        if !path.is_absolute()
            || canonical != *path
            || !canonical.starts_with(&root)
            || canonical == root
            || !canonical.is_dir()
        {
            return Err(BAD_PATHS);
        }
    }
    if paths.profile_id.is_empty()
        || !paths
            .profile_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || paths.workspace_dir != paths.store_dir.join("workspace")
        || paths.tmp_dir != paths.runtime_dir.join("tmp")
    {
        return Err(BAD_PATHS);
    }
    let mut config = DaemonConfig::new(paths.profile_id, paths.store_dir, paths.runtime_dir);
    Endpoint::new(&config.runtime_dir, &config.profile_id)
        .validate_for_bind(&config.runtime_dir)
        .map_err(|_| BAD_PATHS)?;
    // Includes the UDS prefix. The literal 8 MiB budget in draft C1 is invalid.
    config.frame_limit = FRAME_LIMIT;
    config.outbound_queued_bytes = FRAME_LIMIT + 4;
    config.max_connections = 4;
    config.outbound_queue_capacity = 8;
    config.handshake_timeout = Duration::from_secs(10);
    config.drain_timeout = Duration::from_secs(5);
    config.discovery_disabled = true;
    config.lockdown_root_override = Some(paths.workspace_dir);
    let mut slot = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(previous) = slot.as_ref() {
        return if previous.config.profile_id == config.profile_id
            && previous.config.store_dir == config.store_dir
            && previous.config.runtime_dir == config.runtime_dir
        {
            Ok(OK)
        } else {
            Err(BAD_PATHS)
        };
    }
    let vm = env.get_java_vm().map_err(|_| INTERNAL)?;
    let context = env.new_global_ref(app).map_err(|_| INTERNAL)?;
    // SAFETY: this crate is the sole ndk-context owner in the test process.
    // STATE serializes initialization/release; the GlobalRef stays alive until
    // all daemon runtime threads have joined in release().
    unsafe {
        ndk_context::initialize_android_context(
            vm.get_java_vm_pointer().cast(),
            context.as_obj().as_raw().cast(),
        )
    };
    *slot = Some(EmbeddedDaemon {
        _context: context,
        config,
        running: None,
        last: json!({"phase":"Stopped", "daemon_generation":null}),
    });
    Ok(OK)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    dek: JByteArray<'_>,
    policy: JString<'_>,
) -> jint {
    boundary(&mut env, INTERNAL, |env| {
        let status = start(env, dek, policy).unwrap_or_else(|code| code);
        log_status(env, "nativeStart", status)
    })
}

fn start(env: &mut JNIEnv<'_>, dek: JByteArray<'_>, policy: JString<'_>) -> Result<jint, jint> {
    // The spike never reads or stores a DEK. Clear even malformed arrays in
    // bounded chunks; production must copy into Zeroizing before clearing.
    let len = env.get_array_length(&dek).map_err(|_| VAULT_KEY_INVALID)?;
    let zeros = [0i8; 256];
    let mut offset = 0;
    while offset < len {
        let count = (len - offset).min(256);
        env.set_byte_array_region(&dek, offset, &zeros[..count as usize])
            .map_err(|_| INTERNAL)?;
        offset += count;
    }
    if len != 32 {
        return Err(VAULT_KEY_INVALID);
    }
    let text: String = env.get_string(&policy).map_err(|_| BAD_POLICY)?.into();
    let policy: SpikePolicy = serde_json::from_str(&text).map_err(|_| BAD_POLICY)?;
    if !policy.spike_use_default_dependencies {
        return Err(BAD_POLICY);
    }
    let mut slot = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let daemon = slot.as_mut().ok_or(NOT_INITIALIZED)?;
    if daemon.running.is_some() {
        return Err(ALREADY_RUNNING);
    }
    let mut config = daemon.config.clone();
    if let Some(model) = policy.default_model {
        if model.trim().is_empty() {
            return Err(BAD_POLICY);
        }
        config.default_model = model;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(8 * 1024 * 1024)
        .thread_name("haider-android")
        .enable_all()
        .build()
        .map_err(|_| INTERNAL)?;
    let task = {
        let _entered = runtime.enter();
        haider_daemon::spawn_with_dependencies(config, DaemonDependencies::default())
    };
    daemon.running = Some(Running {
        readiness: task.readiness(),
        diagnostics: task.diagnostics(),
        shutdown: task.shutdown_handle(),
        join: Box::pin(task.join()),
        runtime,
        generation: None,
    });
    daemon.last = json!({"phase":"Starting", "daemon_generation":null});
    Ok(OK)
}

// The embedding diagnostics do not expose the durable generation yet. This
// spike-only self-handshake obtains the real value; production needs a typed
// readiness field instead of consuming a connection slot in nativeObserve.
async fn read_generation(path: PathBuf) -> Result<u64, ()> {
    let mut socket = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|_| ())?;
    let fixture = include_bytes!("../../../android/spike/host/src/main/assets/hello.json");
    let hello: haider_rpc::WireFrame = serde_json::from_slice(fixture).map_err(|_| ())?;
    let frame = haider_rpc::uds_codec::encode(&hello, FRAME_LIMIT).map_err(|_| ())?;
    socket.write_all(&frame).await.map_err(|_| ())?;
    let len = socket.read_u32().await.map_err(|_| ())? as usize;
    if len == 0 || len > FRAME_LIMIT {
        return Err(());
    }
    let mut bytes = vec![0; len];
    socket.read_exact(&mut bytes).await.map_err(|_| ())?;
    let welcome: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if welcome["kind"] != "welcome" || welcome["lifecycle_phase"] != "ready" {
        return Err(());
    }
    welcome["daemon_generation"].as_u64().ok_or(())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeObserve(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    boundary(&mut env, std::ptr::null_mut(), |env| {
        let mut slot = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value = match slot.as_mut() {
            None => json!({"phase":"Stopped", "daemon_generation":null}),
            Some(daemon) => observe(daemon),
        };
        java_json(env, value)
    })
}

fn observe(daemon: &mut EmbeddedDaemon) -> Value {
    if daemon.last["error_code"] == "INTERNAL" {
        return daemon.last.clone();
    }
    let Some(running) = daemon.running.as_mut() else {
        return daemon.last.clone();
    };
    let phase = running.readiness.current();
    let snapshot = running.readiness.snapshot();
    let mut value = json!({"phase":format!("{:?}", phase.phase()),
        "daemon_generation":running.generation});
    if phase == DaemonState::Ready {
        if snapshot.ready && running.generation.is_none() {
            running.generation = running.runtime.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    read_generation(daemon.config.endpoint_path()),
                )
                .await
                .ok()
                .and_then(Result::ok)
            });
        }
        let snapshot = running.readiness.snapshot();
        if snapshot.ready && running.generation.is_some() {
            value["daemon_generation"] = json!(running.generation);
            value["ready_since_unix_ms"] = json!(snapshot.ready_since_unix_ms);
            value["endpoint_path"] = json!(daemon.config.endpoint_path());
        } else {
            // Publish Ready only with the complete endpoint/generation facts.
            // A lost admission for our temporary self-handshake is retryable.
            let latest = running.readiness.current();
            value["phase"] = json!(if latest == DaemonState::Ready {
                "Recovering".to_owned()
            } else {
                format!("{:?}", latest.phase())
            });
        }
    }
    if matches!(phase, DaemonState::Failed { .. })
        || running.diagnostics.snapshot().finished && phase != DaemonState::Stopped
    {
        value = failed("DAEMON_FAILED");
    }
    // Exercises the real ConnectivityManager JNI path on a Tokio worker.
    value["spike_route_status"] = json!(running.runtime.block_on(async {
        tokio::spawn(async { format!("{:?}", haider_platform::route_status()) })
            .await
            .unwrap_or_else(|_| "Unknown".into())
    }));
    daemon.last = value.clone();
    value
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeShutdown(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    forced: jboolean,
    deadline_ms: jlong,
) -> jint {
    boundary(&mut env, INTERNAL, |env| {
        let mut slot = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(daemon) = slot.as_mut() else {
            return NOT_INITIALIZED;
        };
        let status = shutdown(daemon, forced != 0, deadline_ms);
        log_status(env, "nativeShutdown", status)
    })
}

fn shutdown(daemon: &mut EmbeddedDaemon, forced: bool, deadline_ms: jlong) -> jint {
    let Some(running) = daemon.running.as_mut() else {
        return OK;
    };
    running.shutdown.request_graceful();
    if forced {
        running.shutdown.request("android-forced");
    }
    let deadline = Duration::from_millis(if deadline_ms <= 0 {
        7000
    } else {
        deadline_ms as u64
    });
    // Keep the consuming join future across a timeout. Dropping it would
    // detach the owner task and prevent a subsequent shutdown from joining it.
    let result = running
        .runtime
        .block_on(async { tokio::time::timeout(deadline, &mut running.join).await });
    let status = match result {
        Err(_) => {
            daemon.last = failed("SHUTDOWN_TIMEOUT");
            return TIMEOUT;
        }
        Ok(Ok(ShutdownOutcome::Graceful)) => OK,
        Ok(Ok(ShutdownOutcome::Forced)) => FORCED,
        Ok(Err(_)) => INTERNAL,
    };
    daemon.last = if status == INTERNAL {
        failed("DAEMON_FAILED")
    } else {
        json!({"phase":"Stopped", "daemon_generation":running.generation, "shutdown_status":status})
    };
    // Runtime drop waits for blocking tasks too, while the global ref lives.
    daemon.running.take();
    status
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeRelease(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) {
    boundary(&mut env, (), |_| {
        let mut slot = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mut daemon) = slot.take() {
            shutdown(&mut daemon, true, 7000);
            drop(daemon);
        }
    });
}
