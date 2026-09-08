#![deny(unsafe_op_in_unsafe_fn)]

use crate::completion::CompletionReceipt;
use crate::contract::{NativeStatus as Status, Observation, Paths, Policy, shutdown_budget};
use haider_daemon::{DaemonConfig, DaemonDependencies, DaemonState, ShutdownOutcome};
use jni::JNIEnv;
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::watch;
use zeroize::Zeroizing;

static PANIC_HOOK: std::sync::Once = std::sync::Once::new();
static STATE: OnceLock<Mutex<Option<EmbeddedDaemon>>> = OnceLock::new();
// Release does not grant permission to switch profiles in the same process.
static PATH_IDENTITY: OnceLock<Paths> = OnceLock::new();
static LAST: OnceLock<Mutex<Observation>> = OnceLock::new();
// Observe/shutdown never contend with JNI initialization, proxy Binder calls,
// filesystem validation, or context release under the singleton mutex.
static LIVE: Mutex<Option<Arc<Running>>> = Mutex::new(None);
fn terminal_observation() -> &'static Mutex<Observation> {
    LAST.get_or_init(|| Mutex::new(Observation::phase("Stopped", 0)))
}

struct ContextOwner(GlobalRef);
impl Drop for ContextOwner {
    fn drop(&mut self) {
        // SAFETY: the singleton initializes ndk-context once per owner, and
        // every runtime reader retains this Arc until runtime teardown joins.
        unsafe { ndk_context::release_android_context() };
    }
}

struct EmbeddedDaemon {
    context: Arc<ContextOwner>,
    paths: Paths,
    debuggable: bool,
    running: Option<Arc<Running>>,
}

struct Probe {
    readiness: haider_daemon::Readiness,
    diagnostics: haider_daemon::DaemonTaskDiagnostics,
}

struct Running {
    stop: watch::Sender<u8>, // 0 idle, 1 graceful, 2 forced; monotonic.
    probe: Mutex<Option<Probe>>,
    last: Mutex<Observation>,
    completion: CompletionReceipt,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn state() -> &'static Mutex<Option<EmbeddedDaemon>> {
    STATE.get_or_init(|| Mutex::new(None))
}

impl Running {
    fn observe(&self) -> Observation {
        let mut last = lock(&self.last);
        if self.completion.get().is_some() || last.error_code.is_some() {
            return last.clone();
        }
        if let Some(probe) = lock(&self.probe).as_ref() {
            let facts = probe.readiness.snapshot();
            let diagnostics = probe.diagnostics.snapshot();
            let generation = diagnostics
                .bootstrap
                .as_ref()
                .map_or(0, |b| b.daemon_generation);
            let phase = match diagnostics.state {
                DaemonState::Starting => "Starting",
                DaemonState::Recovering => "Recovering",
                DaemonState::Ready => {
                    if facts.ready {
                        "Ready"
                    } else {
                        "Recovering"
                    }
                }
                DaemonState::Draining { .. } => "Draining",
                DaemonState::Failed { .. } => "Failed",
                DaemonState::Stopped => "Draining", // owner teardown still has readers
            };
            *last = Observation::phase(phase, generation);
            if facts.ready
                && let Some(bootstrap) = diagnostics.bootstrap
            {
                last.ready_since_unix_ms = facts.ready_since_unix_ms;
                last.endpoint_path = Some(bootstrap.endpoint_path);
            }
        }
        last.clone()
    }

    fn shutdown(&self, forced: bool, deadline: Duration) -> Status {
        self.stop.send_if_modified(|mode| {
            let next = if forced { 2 } else { 1 };
            if *mode < next {
                *mode = next;
                true
            } else {
                false
            }
        });
        let status = self.completion.shutdown_result(deadline);
        if status == Status::ShutdownTimeout {
            let generation = self.observe().daemon_generation;
            *lock(&self.last) = Observation::failed(Status::ShutdownTimeout, generation);
        }
        status
    }
}

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
            let _ = catch_unwind(AssertUnwindSafe(|| {
                if let Some(running) = lock(&LIVE).clone() {
                    let generation = running.observe().daemon_generation;
                    *lock(&running.last) = Observation::failed(Status::Internal, generation);
                }
            }));
            fallback
        }
    }
}

fn java_json(env: &mut JNIEnv<'_>, value: &impl serde::Serialize) -> jstring {
    serde_json::to_string(value)
        .ok()
        .and_then(|text| env.new_string(text).ok())
        .map(JString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

// The six symbols and typed arguments match the frozen static JVM descriptors.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeVersion(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    boundary(&mut env, std::ptr::null_mut(), |env| {
        java_json(
            env,
            &serde_json::json!({
                "jni_version":1, "daemon_version":env!("CARGO_PKG_VERSION"),
                "wire_protocol":haider_rpc::WIRE_PROTOCOL_VERSION,
                "build_id":option_env!("HAIDER_ANDROID_BUILD_ID").unwrap_or(env!("HAIDER_ANDROID_SOURCE_ID")),
                "abi":if cfg!(target_arch = "aarch64") { "arm64-v8a" } else { "x86_64" }
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
    boundary(&mut env, Status::Internal as jint, |env| {
        init(env, context, paths).unwrap_or_else(|s| s) as jint
    })
}

fn init(env: &mut JNIEnv<'_>, context: JObject<'_>, paths: JString<'_>) -> Result<Status, Status> {
    let text: String = env.get_string(&paths).map_err(|_| Status::BadPaths)?.into();
    let paths: Paths = serde_json::from_str(&text).map_err(|_| Status::BadPaths)?;
    let app = env
        .call_method(
            context,
            "getApplicationContext",
            "()Landroid/content/Context;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(|_| Status::Internal)?;
    if app.is_null() {
        return Err(Status::BadArgument);
    }
    let files = env
        .call_method(&app, "getFilesDir", "()Ljava/io/File;", &[])
        .and_then(|v| v.l())
        .map_err(|_| Status::Internal)?;
    let root = env
        .call_method(files, "getCanonicalPath", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(|_| Status::Internal)?;
    let root: String = env
        .get_string(&JString::from(root))
        .map_err(|_| Status::Internal)?
        .into();
    paths.validate(std::path::Path::new(&root))?;
    haider_platform::set_native_temp_directory(&paths.tmp_dir).map_err(|_| Status::BadPaths)?;
    let mut slot = lock(state());
    if let Some(identity) = PATH_IDENTITY.get()
        && identity != &paths
    {
        return Err(Status::BadPaths);
    }
    if let Some(previous) = slot.as_ref() {
        if previous.paths != paths {
            return Err(Status::BadPaths);
        }
        return if env
            .is_same_object(previous.context.0.as_obj(), &app)
            .map_err(|_| Status::Internal)?
        {
            Ok(Status::Ok)
        } else {
            Err(Status::BadArgument)
        };
    }
    PANIC_HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|_| eprintln!("Haider native panic (redacted)")))
    });
    crate::logging::record(&paths.logs_dir, &Observation::phase("Stopped", 0))
        .map_err(|_| Status::Internal)?;
    let info = env
        .call_method(
            &app,
            "getApplicationInfo",
            "()Landroid/content/pm/ApplicationInfo;",
            &[],
        )
        .and_then(|value| value.l())
        .map_err(|_| Status::Internal)?;
    let debuggable = env
        .get_field(info, "flags", "I")
        .and_then(|value| value.i())
        .map_err(|_| Status::Internal)?
        & 2
        != 0;
    let vm = env.get_java_vm().map_err(|_| Status::Internal)?;
    let context = env.new_global_ref(app).map_err(|_| Status::Internal)?;
    // SAFETY: STATE serializes initialization/release. ContextOwner outlives
    // the owner thread, every runtime task, and runtime's blocking-pool join.
    unsafe {
        ndk_context::initialize_android_context(
            vm.get_java_vm_pointer().cast(),
            context.as_obj().as_raw().cast(),
        )
    };
    let _ = PATH_IDENTITY.set(paths.clone());
    *slot = Some(EmbeddedDaemon {
        context: Arc::new(ContextOwner(context)),
        paths,
        debuggable,
        running: None,
    });
    Ok(Status::Ok)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    dek: JByteArray<'_>,
    policy: JString<'_>,
) -> jint {
    let status = boundary(&mut env, Status::Internal as jint, |env| {
        start(env, &dek, policy).unwrap_or_else(|s| s) as jint
    });
    // A finally boundary clears even malformed arrays, bad policy, overlap,
    // JNI exceptions and caught panics. Kotlin also clears its original array.
    let cleared = boundary(&mut env, false, |env| clear_array(env, &dek).is_ok());
    if cleared {
        status
    } else {
        Status::Internal as jint
    }
}

fn clear_array(env: &mut JNIEnv<'_>, array: &JByteArray<'_>) -> Result<(), Status> {
    if array.is_null() {
        return Ok(());
    }
    let length = env.get_array_length(array).map_err(|_| Status::Internal)?;
    let zeros = [0i8; 256];
    for offset in (0..length).step_by(256) {
        env.set_byte_array_region(array, offset, &zeros[..(length - offset).min(256) as usize])
            .map_err(|_| Status::Internal)?;
    }
    Ok(())
}

fn start(
    env: &mut JNIEnv<'_>,
    array: &JByteArray<'_>,
    policy: JString<'_>,
) -> Result<Status, Status> {
    if array.is_null()
        || env
            .get_array_length(array)
            .map_err(|_| Status::VaultKeyInvalid)?
            != 32
    {
        return Err(Status::VaultKeyInvalid);
    }
    let mut bytes = Zeroizing::new([0i8; 32]);
    env.get_byte_array_region(array, 0, &mut *bytes)
        .map_err(|_| Status::VaultKeyInvalid)?;
    let mut dek = Zeroizing::new([0u8; 32]);
    for (to, from) in dek.iter_mut().zip(bytes.iter()) {
        *to = *from as u8;
    }
    let text: String = env
        .get_string(&policy)
        .map_err(|_| Status::BadPolicy)?
        .into();
    let policy = Policy::parse(&text)?;
    if lock(&LIVE)
        .as_ref()
        .is_some_and(|running| running.completion.get().is_none())
    {
        return Err(Status::AlreadyRunning);
    }
    configure_proxy(env)?;
    let mut slot = lock(state());
    let daemon = slot.as_mut().ok_or(Status::NotInitialized)?;
    if daemon
        .running
        .as_ref()
        .is_some_and(|r| r.completion.get().is_none())
    {
        return Err(Status::AlreadyRunning);
    }
    let (stop, receiver) = watch::channel(0);
    let running = Arc::new(Running {
        stop,
        probe: Mutex::new(None),
        last: Mutex::new(Observation::phase("Starting", 0)),
        completion: CompletionReceipt::default(),
    });
    let owner = Arc::clone(&running);
    let context = Arc::clone(&daemon.context);
    let paths = daemon.paths.clone();
    let fake_provider =
        crate::contract::fake_provider_enabled(daemon.debuggable, &paths.runtime_dir);
    *lock(terminal_observation()) = Observation::phase("Starting", 0);
    std::thread::Builder::new()
        .name("haider-android-owner".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let status = catch_unwind(AssertUnwindSafe(|| {
                run(&owner, receiver, &paths, policy, dek, fake_provider)
            }))
            .unwrap_or(Status::Internal);
            // run() has destroyed its runtime (including blocking readers).
            // Retain application context even through an unwinding teardown.
            drop(context);
            let generation = owner.observe().daemon_generation;
            *lock(&owner.probe) = None;
            *lock(&owner.last) = if matches!(status, Status::Ok | Status::ShutdownForced) {
                Observation::phase("Stopped", generation)
            } else {
                Observation::failed(status, generation)
            };
            let final_observation = lock(&owner.last).clone();
            let _ = crate::logging::record(&paths.logs_dir, &final_observation);
            *lock(terminal_observation()) = final_observation;
            owner.completion.finish(status);
        })
        .map_err(|_| Status::Internal)?;
    *lock(&LIVE) = Some(Arc::clone(&running));
    daemon.running = Some(running);
    Ok(Status::Ok)
}

fn run(
    owner: &Running,
    mut stop: watch::Receiver<u8>,
    paths: &Paths,
    policy: Policy,
    dek: Zeroizing<[u8; 32]>,
    fake_provider: bool,
) -> Status {
    let vault = match haider_accounts::EncryptedFileVault::new(paths.store_dir.join("vault"), dek) {
        Ok(vault) => Arc::new(vault),
        Err(_) => return Status::VaultKeyInvalid,
    };
    if vault.authenticate_existing().is_err() {
        return Status::VaultKeyInvalid;
    }
    let mut config = DaemonConfig::new(&paths.profile_id, &paths.store_dir, &paths.runtime_dir);
    config.default_model = policy.default_model;
    config.store_synchronous = Some(policy.store_synchronous);
    config.android_workspace_dir = Some(paths.workspace_dir.clone());
    config.frame_limit = 8_388_608;
    config.outbound_queued_bytes = 8_388_612;
    config.max_connections = 4;
    config.outbound_queue_capacity = 8;
    config.handshake_timeout = Duration::from_secs(10);
    config.drain_timeout = Duration::from_secs(5);
    config.discovery_disabled = true;
    config.lockdown_root_override = Some(paths.store_dir.join("lockdown"));
    let mut dependencies = DaemonDependencies::default();
    dependencies.accounts.vault = haider_daemon::VaultProvision::Available(vault);
    if fake_provider {
        eprintln!("Haider TEST MODE: deterministic fake provider enabled for this debuggable app");
        crate::test_provider::install(&mut dependencies);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_stack_size(8 * 1024 * 1024)
        .thread_name("haider-android")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return Status::Internal,
    };
    let status = runtime.block_on(async {
        let task = haider_daemon::spawn_with_dependencies(config, dependencies);
        let shutdown = task.shutdown_handle();
        *lock(&owner.probe) = Some(Probe { readiness: task.readiness(), diagnostics: task.diagnostics() });
        let join = task.join();
        tokio::pin!(join);
        let mut logging_tick = tokio::time::interval(Duration::from_millis(250));
        let mut logged_phase = "";
        loop {
            let mode = *stop.borrow_and_update();
            if mode > 0 { shutdown.request_graceful(); }
            if mode > 1 { shutdown.request("android-forced"); }
            tokio::select! {
                _ = logging_tick.tick() => {
                    let observation = owner.observe();
                    if observation.phase != logged_phase {
                        let _ = crate::logging::record(&paths.logs_dir, &observation);
                        logged_phase = observation.phase;
                    }
                }
                result = &mut join => break match result {
                    Ok(ShutdownOutcome::Graceful) => Status::Ok,
                    Ok(ShutdownOutcome::Forced) => Status::ShutdownForced,
                    Err(haider_daemon::DaemonError::Store(_)) => Status::StoreRecoveryFailed,
                    Err(haider_daemon::DaemonError::AlreadyRunning { .. }) => Status::AlreadyRunning,
                    Err(_) => Status::Internal,
                },
                changed = stop.changed() => { if changed.is_err() { shutdown.request_graceful(); shutdown.request("android-owner-lost"); } }
            }
        }
    });
    drop(runtime);
    status
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeObserve(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    boundary(&mut env, std::ptr::null_mut(), |env| {
        let running = lock(&LIVE).clone();
        java_json(
            env,
            &running.map_or_else(|| lock(terminal_observation()).clone(), |r| r.observe()),
        )
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeShutdown(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    forced: jboolean,
    deadline_ms: jlong,
) -> jint {
    boundary(&mut env, Status::Internal as jint, |_| {
        let deadline = match shutdown_budget(deadline_ms) {
            Ok(deadline) => deadline,
            Err(status) => return status as jint,
        };
        let running = lock(&LIVE).clone();
        running.map_or(Status::Ok, |r| r.shutdown(forced != 0, deadline)) as jint
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeRelease(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) {
    boundary(&mut env, (), |_| {
        let running = lock(&LIVE).clone();
        if let Some(running) = running {
            if running.shutdown(true, Duration::from_millis(7000)) == Status::ShutdownTimeout {
                return;
            }
            let mut slot = lock(state());
            // A concurrent start cannot have its new context released here.
            if slot
                .as_ref()
                .and_then(|d| d.running.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, &running))
            {
                *lock(&LIVE) = None;
                slot.take();
            }
        } else {
            let mut slot = lock(state());
            if slot.as_ref().is_some_and(|d| d.running.is_none()) {
                slot.take();
            }
        }
    });
}

fn configure_proxy(env: &mut JNIEnv<'_>) -> Result<(), Status> {
    let slot = lock(state());
    let daemon = slot.as_ref().ok_or(Status::NotInitialized)?;
    let service_name = env
        .new_string("connectivity")
        .map_err(|_| Status::Internal)?;
    let manager = env
        .call_method(
            daemon.context.0.as_obj(),
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&service_name)],
        )
        .and_then(|v| v.l())
        .map_err(|_| Status::Internal)?;
    let proxy = env
        .call_method(manager, "getDefaultProxy", "()Landroid/net/ProxyInfo;", &[])
        .and_then(|v| v.l())
        .map_err(|_| Status::Internal)?;
    if proxy.is_null() {
        return haider_platform::set_android_http_proxy(None, 0).map_err(|_| Status::Internal);
    }
    let host = env
        .call_method(&proxy, "getHost", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(|_| Status::Internal)?;
    let port = env
        .call_method(&proxy, "getPort", "()I", &[])
        .and_then(|v| v.i())
        .map_err(|_| Status::Internal)?;
    let host: String = env
        .get_string(&JString::from(host))
        .map_err(|_| Status::Internal)?
        .into();
    let port = u16::try_from(port)
        .ok()
        .filter(|port| *port > 0)
        .ok_or(Status::Internal)?;
    if host.trim().is_empty() || host.contains(['/', '@', '?', '#']) {
        return Err(Status::Internal);
    }
    haider_platform::set_android_http_proxy(Some(&host), port).map_err(|_| Status::Internal)
}
