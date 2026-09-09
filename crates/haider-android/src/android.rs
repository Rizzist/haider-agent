#![deny(unsafe_op_in_unsafe_fn)]

use crate::contract::{NativeStatus as Status, Paths, Policy, shutdown_budget};
use crate::owner::{Host, Running, run_daemon};
use haider_daemon::{DaemonConfig, DaemonDependencies};
use jni::JNIEnv;
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::watch;
use zeroize::Zeroizing;

static PANIC_HOOK: std::sync::Once = std::sync::Once::new();
static HOST: OnceLock<Arc<Host<ContextOwner>>> = OnceLock::new();
fn host() -> &'static Arc<Host<ContextOwner>> {
    HOST.get_or_init(|| Arc::new(Host::new()))
}

struct ContextOwner(GlobalRef);
impl Drop for ContextOwner {
    fn drop(&mut self) {
        // SAFETY: the singleton initializes ndk-context once per owner, and
        // every runtime reader retains this Arc until runtime teardown joins.
        unsafe { ndk_context::release_android_context() };
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
                if let Some(host) = HOST.get() {
                    host.fail_boundary();
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
                "build_id":env!("HAIDER_ANDROID_SOURCE_ID"),
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
    let logs = paths.logs_dir.clone();
    host().initialize(paths, |previous| {
        if let Some(previous) = previous {
            return if env
                .is_same_object(previous.0.as_obj(), &app)
                .map_err(|_| Status::Internal)?
            {
                Ok(None)
            } else {
                Err(Status::BadArgument)
            };
        }
        PANIC_HOOK.call_once(|| {
            std::panic::set_hook(Box::new(|_| eprintln!("Haider native panic (redacted)")))
        });
        crate::logging::record(&logs, &crate::contract::Observation::phase("Stopped", 0))
            .map_err(|_| Status::Internal)?;
        let vm = env.get_java_vm().map_err(|_| Status::Internal)?;
        let context = env.new_global_ref(app).map_err(|_| Status::Internal)?;
        // SAFETY: Host serializes initialization/release. ContextOwner outlives
        // the native owner and every reader, including blocking-pool teardown.
        unsafe {
            ndk_context::initialize_android_context(
                vm.get_java_vm_pointer().cast(),
                context.as_obj().as_raw().cast(),
            )
        };
        Ok(Some(ContextOwner(context)))
    })
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
    if host().is_running() {
        return Err(Status::AlreadyRunning);
    }
    let proxy = read_proxy(env)?;
    host().start(move |owner, receiver, paths| run(owner, receiver, paths, policy, dek, proxy))
}

fn run(
    owner: &Running,
    stop: watch::Receiver<u8>,
    paths: &Paths,
    policy: Policy,
    dek: Zeroizing<[u8; 32]>,
    proxy: Option<ProxyConfiguration>,
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
    run_daemon(
        owner,
        stop,
        async {
            let configured = match proxy {
                Some(proxy) => {
                    haider_platform::set_android_http_proxy(
                        Some(&proxy.host),
                        proxy.port,
                        proxy.exclusions,
                    )
                    .await
                }
                None => haider_platform::set_android_http_proxy(None, 0, Vec::new()).await,
            };
            configured.map_err(|_| Status::Internal)?;
            Ok(haider_daemon::spawn_with_dependencies(config, dependencies))
        },
        paths,
    )
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeObserve(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jstring {
    boundary(&mut env, std::ptr::null_mut(), |env| {
        java_json(env, &host().observe())
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
        host().shutdown(forced != 0, deadline) as jint
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_diffforge_haider_daemon_NativeDaemon_nativeRelease(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) {
    boundary(&mut env, (), |_| {
        host().release(Duration::from_millis(7000));
    });
}

struct ProxyConfiguration {
    host: String,
    port: u16,
    exclusions: Vec<String>,
}

fn read_proxy(env: &mut JNIEnv<'_>) -> Result<Option<ProxyConfiguration>, Status> {
    host().with_context(|context| {
        let service_name = env
            .new_string("connectivity")
            .map_err(|_| Status::Internal)?;
        let manager = env
            .call_method(
                context.0.as_obj(),
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
            return Ok(None);
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
        let exclusions = env
            .call_method(&proxy, "getExclusionList", "()[Ljava/lang/String;", &[])
            .and_then(|v| v.l())
            .map_err(|_| Status::Internal)?;
        let exclusions = jni::objects::JObjectArray::from(exclusions);
        let count = env
            .get_array_length(&exclusions)
            .map_err(|_| Status::Internal)?;
        let mut excluded_hosts = Vec::new();
        for index in 0..count {
            let name = env
                .get_object_array_element(&exclusions, index)
                .map_err(|_| Status::Internal)?;
            let name = JString::from(name);
            excluded_hosts.push(env.get_string(&name).map_err(|_| Status::Internal)?.into());
            env.delete_local_ref(name).map_err(|_| Status::Internal)?;
        }
        Ok(Some(ProxyConfiguration {
            host,
            port,
            exclusions: excluded_hosts,
        }))
    })
}
