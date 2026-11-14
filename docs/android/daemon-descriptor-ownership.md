# Daemon descriptor ownership

Every descriptor opened for a daemon generation has an owner that closes it
before that generation's completion receipt permits another start. A pathname
left behind after a crash is not an open descriptor. Cleanup must also preserve
unrelated callers' open files and successor generations' socket nodes.

| Resource / open path | Owner and close boundary |
| --- | --- |
| Kotlin lifecycle `state-v1.json` reads; vault `wrapped-vault-dek` reads | `PrivateDaemonFiles.withDescriptor`: `Os.close` in `finally`, including failed `fstat`, size validation, read, or decoding in the caller. |
| Kotlin atomic-write temporary file | The same helper closes after all writes and file `fsync`, before rename. Failed publication removes only its temporary pathname. |
| Kotlin atomic-write parent directory | The same helper closes after directory validation and `fsync`. All three Kotlin open variants use `O_CLOEXEC` and retain `O_NOFOLLOW`. |
| Profile `lock`, transient `lock.owner` publication and directory handles | `haider-store::ProfileLock` owns its `File`; store close drops it last. Owner-record and directory publication handles are scoped RAII values. |
| SQLite database, journal/WAL and shared-memory handles | `Store` / SQLite connection under the shared `StoreOwner`; `SqliteStoreHandle::close` takes and closes the store on the blocking pool. Early startup failure drops the lease/connection. |
| Store CAS reads, upload staging files and durable-publication directories | Scoped `File` / owned descriptor values; `CasUpload` owns unfinished uploads. Generation-owned tasks must finish or be cancelled and joined before store close. |
| Runtime directory / ancestor handles; PID and completion-receipt publications | `RuntimeDirectory` and platform `PreparedRuntimeDirectory` own directory descriptors; atomic writers own scoped files. Endpoint teardown drops the generation's handles. |
| RPC `h.sock` listener and accepted sockets (Windows named pipes) | `BoundEndpoint` and `ConnectionRuntime`; stop accepting, drain or abort **and join** all connection/writer tasks, clean up the owned endpoint, then drop listener. |
| Standalone `mobile.sock`; legacy mobile TCP listener, clients and bootstrap files | `MobileTransportServer`; `shutdown` joins accept/client tasks. Its `Drop` cancels remaining tasks. Bootstrap-file handles are scoped. Kotlin socket clients close each attempt and stop with the service generation. |
| Native `native.log` and its parent directory; rotated log files | `haider-android::logging::record` owns scoped `File` / `OwnedFd` values on every successful or failed call. Rotation uses descriptor-relative renames, without retaining extra file handles. Desktop timing telemetry writes to the caller-owned stderr handle. |
| Rust `effect-diagnostics.jsonl` | `EffectDiagnostics` owns its file behind a mutex. Its generation's runtime/worker context owns the shared diagnostics value and drops it after workers retire. |
| Launcher liveness pipe; Windows peer-exit handles | `DaemonLivenessWatcher` and connection-owned `PeerExitWatcher`; the signal runner aborts and joins the liveness task on completion. Embedded JNI has no launcher pipe. |
| Runtime reactor / wakeup descriptors (kqueue, epoll, eventfd or socket pair) | JNI `run_daemon` destroys its Tokio runtime before publishing completion. A caller-supplied desktop runtime is owned by that caller and may outlive the daemon. |
| Watchers: account/profile fault, roster/session subscriptions, file-monitor reads and shell-backed monitors | Generation-owned hub/monitor tasks and subscriptions. Channel/watch subscriptions do not open OS descriptors; scoped file readers and child process pipes close when their joined tasks retire. |
| Optional provider HTTP/OAuth sockets, SSH sockets, subprocess stdin/stdout/stderr, native sidecar pipes, hook-server pipes | Their generation-owned client, actor, worker or supervisor. Runtime teardown shuts down these owners, aborts/joins cancellable work, and awaits non-abortable vault publication before releasing the profile. |
| Vault/account ciphertext reads and atomic writes; lockdown state/lock files | `EncryptedFileVault` owns its pinned directory for the generation; file operations and publication directories use scoped handles. The account actor/runtime context drops the vault after its work retires. Lockdown state and lock file operations own scoped files. Secret bytes remain under existing zeroizing ownership; the fd fix does not change persisted formats. |

Process-wide Android Binder, framework, Keystore, logcat and UI descriptors are
owned by their process-level services. They are not recreated by native daemon
generations. The immutable Android workspace ceiling and global lockdown manager
also retain their pinned workspace/sandbox directory handles once per process;
these security anchors are shared by all generations at the same fixed paths.
The native probe warms process-level platform initialization once,
then requires an identical `/dev/fd` count after every subsequent generation.
It runs in an isolated test subprocess to avoid counting parallel tests' files.
It covers graceful and forced shutdown, abrupt in-process crash, failed
bootstrap, and failed startup after store open. A caller-owned file and a
preserved endpoint obstruction are negative controls. Android instrumentation separately pins the real
`Os` private-store paths, including validation and publication failures.

Android's `FileInputStream(FileDescriptor)` and `FileOutputStream(FileDescriptor)`
borrow the supplied handle. Their `close` methods do not close it. Private-file
operations therefore use `Os.read` / `Os.write` and one explicit descriptor
owner rather than mixing borrowed streams with an `Os.open` owner. See the
[Android FileInputStream contract](https://developer.android.com/reference/java/io/FileInputStream#FileInputStream(java.io.FileDescriptor)).
