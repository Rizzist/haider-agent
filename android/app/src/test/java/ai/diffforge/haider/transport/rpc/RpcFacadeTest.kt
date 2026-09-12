package ai.diffforge.haider.transport.rpc

import ai.diffforge.haider.ui.accounts.AccountResult
import ai.diffforge.haider.ui.accounts.AuthKind
import ai.diffforge.haider.ui.accounts.CustomModelsProbe
import ai.diffforge.haider.ui.accounts.ModelInventoryAuthority
import ai.diffforge.haider.ui.accounts.OAuthFlow as UiFlow
import ai.diffforge.haider.ui.accounts.OAuthStatus as UiStatus
import ai.diffforge.haider.ui.daemon.*
import ai.diffforge.haider.ui.state.PermissionMode
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test
import java.io.Closeable
import java.io.IOException
import java.net.ServerSocket
import java.net.Socket
import java.nio.file.Files
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger

class RpcFacadeTest {
    private fun snapshot(generation: Long = 1) = DaemonServiceSnapshot(true, DaemonPhase.Ready, "0.0.970", "0.0.970", 1,
        generation, RpcEndpoint("/private/h.sock", 1, generation), 0, null, NetworkState.Available, true, false,
        null, false, 1, 10, 200)
    private class Daemon(private val owner: CoroutineScope, private val deferredMethod: String? = null,
        private val responseGate: CompletableDeferred<Unit>? = null,
        /** The real Hello advertises features; the facade's Loom door reads them (971-V F8). */
        private val features: List<String> = listOf("loom_v1"),
        private val response: (JsonObject) -> JsonObject) : Closeable {
        private val listener = ServerSocket(0)
        private val peers = CopyOnWriteArrayList<Socket>()
        val requests = CopyOnWriteArrayList<JsonObject>()
        private val server = owner.launch(Dispatchers.IO) {
            try { while (isActive) {
                val peer = listener.accept(); peers += peer
                launch {
                    peer.use {
                        try {
                            RpcWire.read(peer.getInputStream())!!
                            writePeer(peer, obj("v" to 1, "kind" to "welcome", "protocol" to 1,
                                "instance_id" to "facade", "daemon_generation" to 1, "frame_limit" to RpcWire.MAX_BODY,
                                "daemon_version" to "0.0.970", "profile_id" to "android-default",
                                "capabilities_granted" to JsonArray(listOf("view", "control").map(::JsonPrimitive)),
                                "features" to JsonArray(features.map(::JsonPrimitive))))
                            while (isActive) {
                                val frame = RpcWire.read(peer.getInputStream()) ?: break
                                if (frame.string("kind") == "ping") {
                                    writePeer(peer, RpcWire.pong(RpcWire.nonce(frame))); continue
                                }
                                val body = if (frame.string("kind") == "menu_answer") JsonObject(frame + ("method" to JsonPrimitive("menu.answer"))) else frame.objectAt("body")
                                requests += body
                                val reply = obj("v" to 1, "kind" to "response", "request_id" to frame.string("request_id"), "body" to response(body))
                                if (body.string("method") == deferredMethod) launch {
                                    responseGate!!.await()
                                    try { writePeer(peer, reply) } catch (_: IOException) { }
                                } else writePeer(peer, reply)
                            }
                        } catch (_: IOException) { }
                    }
                }
            } } catch (_: IOException) { }
        }
        private fun writePeer(peer: Socket, frame: JsonObject) = synchronized(peer) { RpcWire.write(peer.getOutputStream(), frame) }
        fun push(frame: JsonObject) { peers.filter { !it.isClosed }.forEach { writePeer(it, frame) } }
        fun socket(): RpcSocket = object : RpcSocket {
            private val socket = Socket()
            override val input get() = socket.getInputStream()
            override val output get() = socket.getOutputStream()
            override fun connect(path: String) = socket.connect(java.net.InetSocketAddress("127.0.0.1", listener.localPort))
            override fun readTimeout(milliseconds: Int) { socket.soTimeout = milliseconds }
            override fun close() = socket.close()
        }
        override fun close() { listener.close(); peers.forEach { it.close() }; server.cancel() }
    }
    private fun provider() = obj("provider" to "p", "enabled" to true, "availability" to "available", "api_family" to "openai_responses",
        // A router-style catalog: advisory is what licenses a free-text model id.
        "inventory_authority" to "advisory",
        "models" to JsonArray(listOf(JsonPrimitive("m"))), "default_model" to "m", "auth_methods" to JsonArray(listOf("api_key", "oauth").map(::JsonPrimitive)),
        "model_details" to JsonArray(listOf(obj("name" to "m", "context_window" to 4096, "supported_efforts" to JsonArray(listOf(JsonPrimitive("high")))))))
    private fun account() = obj("alias" to "work", "provider" to "p", "auth_method" to "oauth", "active" to true,
        "identity" to "synthetic", "status" to obj("status" to "ok"))
    private fun summary(id: String = "s") = obj("session_id" to id, "head_seq" to 2, "worker_generation" to 7,
        "provider" to "p", "last_model" to "m", "run_state" to "running", "run_id" to "run", "effort" to "high", "fast" to false,
        "needs_input" to obj("kind" to "secret", "title" to "Synthetic secret", "menu_id" to "menu", "request_seq" to 5,
            "worker_generation" to 7, "secret_answer" to true, "options" to JsonArray(listOf(obj("key" to "ok", "label" to "OK")))))
    private fun response(body: JsonObject): JsonObject {
        val method = body.string("method")
        return when (method) {
            "session.list_watch", "account.list_watch" -> obj("method" to method, "accepted" to true)
            "session.list" -> obj("method" to method, "sessions" to JsonArray(listOf(summary(), summary("child"))))
            "provider.list" -> obj("method" to method, "providers" to JsonArray(listOf(provider())), "revision" to 12)
            "account.list" -> obj("method" to method, "descriptors" to JsonArray(listOf(account())), "revision" to 9)
            "session.attach" -> obj("method" to method, "attachment_id" to "attachment-${body.string("session_id")}",
                "attach_state" to obj("session_id" to body.string("session_id"), "replay_through_seq" to 2, "worker_generation" to 7))
            "session.detach" -> obj("method" to method, "attachment_id" to body.string("attachment_id"))
            "session.observe" -> obj("method" to method, "digest" to obj("session_id" to "s", "head_seq" to 2, "worker_generation" to 7, "main_head_seq" to 2, "main_head_node_id" to "actual-node"))
            "session.read" -> {
                val seq = body.objectAt("range").number("start_seq")
                obj("method" to method, "result" to obj("session_id" to body.string("session_id"), "head_seq" to 2,
                    "envelopes" to JsonArray(listOf(obj("session_id" to body.string("session_id"), "seq" to seq, "render" to obj("ui" to true),
                        "payload" to if (seq == 1L) obj("type" to "user_message", "text" to "find needle") else obj("type" to "item", "event" to "completed", "item_id" to "answer",
                            "item" to obj("item" to "agent_message", "text" to "found needle")))))))
            }
            "session.create", "session.fork" -> obj("method" to method, "session_id" to "child", "created_seq" to 1, "worker_generation" to 7)
            "session.rename" -> obj("method" to method, "session_id" to "s", "renamed_seq" to 3, "worker_generation" to 7)
            "session.seen" -> obj("method" to method, "session_id" to "s", "seen_seq" to 3, "worker_generation" to 7)
            "session.select_model", "session.select_effort" -> obj("method" to method, "session_id" to "s", "selected_seq" to 3, "worker_generation" to 7)
            "turn.submit" -> obj("method" to method, "session_id" to "s", "accepted_seq" to 3, "worker_generation" to 7, "run_id" to "run")
            "turn.cancel" -> obj("method" to method, "session_id" to "s", "run_id" to "run", "status" to "accepted")
            "menu.answer" -> obj("method" to method, "resolution_seq" to 6)
            "vault.stage" -> obj("method" to method, "vault_reference" to "synthetic-stage", "expires_at_ms" to System.currentTimeMillis() + 300_000)
            "provider.models_refresh" -> obj("method" to method, "provider" to provider(), "revision" to 13)
            // Receipt-free CAS ingress; the address is what a turn then names.
            "artifact.put" -> obj("method" to method, "artifact" to "blake3:synthetic-artifact",
                "bytes" to body.string("data_base64").length)
            "provider.models_probe" -> obj("method" to method, "provider" to body.string("provider"),
                "models" to JsonArray(listOf("probe-b", "probe-a").map(::JsonPrimitive)), "default_model" to "probe-b")
            "provider.configure" -> obj("method" to method, "provider" to provider(), "revision" to 14)
            "loom.list" -> obj("method" to method, "revision" to 3,
                "agent_types" to JsonArray(emptyList()), "workflows" to JsonArray(emptyList()))
            "account.login_api", "account.add", "account.set_active", "account.refresh" -> obj("method" to method, "descriptor" to account())
            "account.remove" -> obj("method" to method, "removed_alias" to "work", "revision" to 10)
            "account.oauth_start" -> obj("method" to method, "availability" to obj("available" to true), "flow_id" to "synthetic-flow", "authorization_url" to "https://example.invalid/authorize")
            "account.oauth_status" -> obj("method" to method, "status" to obj("status" to "ready", "oauth_reference" to "synthetic-ready", "identity" to "synthetic"))
            "account.oauth_cancel" -> obj("method" to method, "status" to obj("status" to "cancelled"))
            else -> error("unexpected method $method")
        }
    }

    @Test fun facadeBindsCurrentUiNamesAndUsesAuthoritativeCoordinates() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val controls = mutableSetOf<String>()
        val daemon = Daemon(owner) { body ->
            val method = body.string("method")
            if (method == "session.attach" && body.string("mode") == "control") controls += body.string("session_id")
            if (method == "session.detach") controls -= body.string("attachment_id").removePrefix("attachment-")
            val guarded = setOf("session.seen", "session.rename", "session.fork", "session.select_model", "session.select_effort", "turn.submit", "turn.cancel", "menu.answer")
            if (method in guarded && body.string("session_id") !in controls)
                obj("method" to "error", "code" to "capability_denied") else response(body)
        }
        val client = RpcClient(owner, daemon::socket)
        val directory = Files.createTempDirectory("facade").toFile()
        val starts = AtomicInteger(); val stops = AtomicInteger(); val restarts = AtomicInteger()
        val permission = MutableStateFlow<Pair<Boolean, Boolean>?>(null)
        val binder = object : RpcControlPlane {
            override val snapshots = MutableStateFlow<DaemonServiceSnapshot?>(snapshot())
            override suspend fun start() { starts.incrementAndGet() }
            override suspend fun stop() { stops.incrementAndGet() }
            override suspend fun restart() { restarts.incrementAndGet() }
            override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) {
                permission.value = granted to permanentlyDenied
            }
        }
        val service = RpcDaemonService(owner, binder, directory, "/private/workspaces", "p", "m", 4096, client)
        val ui: DaemonService = service
        try {
            withTimeout(5000) { ui.sessions.first { it.size == 2 } }
            ui.start(); ui.stop(); ui.restart()
            ui.reportNotificationPermission(false, true)
            assertEquals(false to true, permission.value)
            ui.reportNotificationPermission(true, false)
            assertEquals(true to false, permission.value)
            assertEquals(listOf(1, 1, 1), listOf(starts.get(), stops.get(), restarts.get()))
            ui.markSeen("s") // Drawer action before any selected-session attachment.
            ui.activate("s")
            withTimeout(5000) { ui.models.first { it != null } }
            assertFalse(ui.shell.value.available)
            assertEquals("process_exec_disabled", ui.shell.value.reason)
            // Existing sessions start on Ask, and BOTH modes are offered: Auto
            // is this client's standing consent, not a daemon setting, so it is
            // available without a wire door (971-V F3).
            assertEquals(PermissionMode.Ask, ui.permissionMode.value)
            assertEquals(setOf(PermissionMode.Ask, PermissionMode.Auto), ui.supportedPermissionModes)
            ui.setPermissionMode(PermissionMode.Auto)
            assertEquals(PermissionMode.Auto, ui.permissionMode.value)
            ui.setPermissionMode(PermissionMode.Ask)
            assertEquals(PermissionMode.Ask, ui.permissionMode.value)
            // No invented method: the mode is never pushed to the daemon.
            assertFalse(daemon.requests.any { it.string("method") == "tool.policy" })
            assertFalse(daemon.requests.any { it.string("method").contains("permission_mode") })
            // A secret card is never answered by standing consent, whatever the
            // mode says, so the fixture session stays parked on its own menu.
            assertNotNull(ui.sessions.value.first { it.id == "s" }.needsInput)
            assertEquals(listOf("high"), ui.providers.value.effortsFor("p", "m"))
            assertEquals(4096L, ui.providers.value.model("p", "m")!!.contextWindow)
            assertTrue(ui.providers.value.effortsFor("p", "absent").isEmpty())
            assertEquals(12L, ui.providers.value.revision)
            assertEquals("m", ui.models.value!!.current.model)
            assertEquals(false, ui.sessions.value.first().fast)
            assertFalse(ui.paging.value.hasMore); assertNull(ui.paging.value.cursor)
            val before = daemon.requests.size; ui.loadMoreSessions(); assertEquals(before, daemon.requests.size)
            ui.selectModel("p", "m")
            assertFalse(daemon.requests.last { it.string("method") == "session.select_model" }.containsKey("confirm_new_epoch"))
            ui.selectModel("p", "m", confirmNewEpoch = true)
            assertEquals(JsonPrimitive(true), daemon.requests.last { it.string("method") == "session.select_model" }["confirm_new_epoch"])
            ui.selectEffort("high")
            assertFalse(daemon.requests.last { it.string("method") == "session.select_effort" }.containsKey("confirm_new_epoch"))
            ui.selectEffort("high", confirmNewEpoch = true)
            assertEquals(JsonPrimitive(true), daemon.requests.last { it.string("method") == "session.select_effort" }["confirm_new_epoch"])

            ui.markSeen("s"); ui.rename("s", "renamed"); ui.stopTurn("s")
            val cancelled = daemon.requests.last { it.string("method") == "turn.cancel" }
            assertEquals("run", cancelled.string("run_id")); assertEquals(7L, cancelled.number("worker_generation"))
            val secret = "synthetic-only".toCharArray()
            val reference = ui.stageMenuSecret(secret)
            assertTrue(secret.all { it == '\u0000' })
            val coordinates = MenuCoordinates("s", "menu", 5, 7, "stable-menu-command")
            ui.answer(coordinates, "ok", 0, MenuAnswerInput.Secret(reference))
            assertEquals("stable-menu-command", daemon.requests.last { it.string("method") == "menu.answer" }.string("command_id"))
            ui.selectModel("p", "m"); ui.selectEffort("high"); ui.selectProvider("p"); ui.refreshModels()
            ui.send("s", "hello")
            assertEquals("steer", daemon.requests.last { it.string("method") == "turn.submit" }.string("mode"))
            ui.send("s", "queued", mode = Delivery.Queue)
            assertEquals("queue", daemon.requests.last { it.string("method") == "turn.submit" }.string("mode"))
            // Attachments: `artifact.put` puts the bytes in the daemon's CAS and
            // `turn.submit` names the address it returned. Round 11 refused every
            // nonempty list before it reached the wire (971-V F2).
            val png = byteArrayOf(0x89.toByte(), 0x50, 0x4E, 0x47)
            val staged = ui.stageAttachment(png, "image/png", null)
            assertEquals(Attachment.Image("blake3:synthetic-artifact", "image/png"), staged)
            assertTrue(daemon.requests.any { it.string("method") == "artifact.put" })
            // The bytes ride base64, never raw, and never as a path.
            assertEquals(java.util.Base64.getEncoder().encodeToString(png),
                daemon.requests.last { it.string("method") == "artifact.put" }.string("data_base64"))
            // The composer's own thumbnail comes from what it just handed over:
            // this wire has no artifact GET door to read it back with.
            assertArrayEquals(png, ui.attachmentBytes("blake3:synthetic-artifact"))
            assertNull(ui.attachmentBytes("blake3:never-staged"))
            ui.send("s", "look at this", listOf(staged!!))
            val withBlock = daemon.requests.last { it.string("method") == "turn.submit" }
            val block = (withBlock["attachments"] as JsonArray).single().jsonObject
            assertEquals("image", block.string("kind"))
            assertEquals("blake3:synthetic-artifact", block.string("artifact"))
            assertEquals("image/png", block.string("mime"))
            // A kind whose facts this client cannot state is refused here, with
            // its own code, rather than reported as "too large" (971-V F2).
            val refused = runCatching { ui.stageAttachment(png, "application/pdf", "fixture.pdf") }.exceptionOrNull()
            assertEquals(AttachmentLimits.KIND_UNSUPPORTED, (refused as? AttachmentRefused)?.code)
            assertFalse(ui.queue.value.supported)
            assertFalse(ui.usage.value.supported)
            assertTrue(daemon.requests.filter { it.string("method") == "session.attach" }.all { it.string("mode") == "control" })
            val transcript = ui.transcript("s")
            assertTrue(transcript is TranscriptLoad.Complete)
            assertEquals(listOf("find needle", "found needle"), transcript.messages.map { it.text })
            val search = ui.search("needle")
            assertTrue(search.complete); assertEquals(4, search.hits.size)
            assertEquals("child", ui.fork("s"))
            val fork = daemon.requests.last { it.string("method") == "session.fork" }
            assertEquals("actual-node", fork.string("fork_node_id")); assertEquals(2L, fork.number("fork_seq"))
            assertEquals("child", ui.createSession())
            binder.snapshots.value = null
            withTimeout(3000) { client.state.first { it == RpcConnectionState.DISCONNECTED } }
            assertEquals(DaemonStatus.Stopped, withTimeout(3000) { ui.status.first { it == DaemonStatus.Stopped } })
        } finally { service.close(); daemon.close(); owner.cancel(); directory.deleteRecursively() }
    }

    @Test fun accountAdapterKeepsCallerAttemptAndEpochWithTruthfulUnavailableStates() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val daemon = Daemon(owner, response = ::response)
        val client = RpcClient(owner, daemon::socket)
        val source = AccountsRepository(client, owner)
        val adapter = RpcAccountsRepository(client, source, owner)
        val ui: ai.diffforge.haider.ui.accounts.AccountsRepository = adapter
        try {
            assertNull(ui.snapshot.value.revision)
            client.connect(RpcUiMapping.target(snapshot())!!)
            ui.refresh(); ui.refreshProviders()
            withTimeout(3000) { ui.snapshot.first { it.revision == 9L } }
            withTimeout(3000) { ui.providers.first { it.isNotEmpty() } }
            assertEquals(listOf("high"), ui.providers.value.single().modelDetails["m"]!!.supportedEfforts)
            val key = "synthetic-only".toCharArray()
            assertEquals(AccountResult.Ok, ui.addApiKey("p", "work", key, replaceExisting = true))
            assertTrue(key.all { it == '\u0000' })
            assertEquals(JsonPrimitive(true), daemon.requests.last { it.string("method") == "account.login_api" }["replace_existing"])
            val calls = daemon.requests.size
            assertEquals(AccountResult.Failed("validate_only_unavailable"), ui.validateApiKey("p", key))
            // Only the adapter's validate-only operation emits no request; background refresh is allowed.
            assertFalse(daemon.requests.drop(calls).any { it.string("method") in setOf("vault.stage", "account.login_api") })
            assertEquals(AccountResult.Ok, ui.remove("work"))
            assertEquals(9L, daemon.requests.last { it.string("method") == "account.remove" }.number("expected_revision"))
            assertEquals(AccountResult.Ok, ui.setActive("work"))
            assertFalse(daemon.requests.last { it.string("method") == "account.set_active" }.containsKey("confirm_new_epoch"))
            val flow = ui.startOAuth("p", null, "caller-attempt") as UiFlow.Started
            val start = daemon.requests.last { it.string("method") == "account.oauth_start" }
            assertEquals("caller-attempt", start.string("attempt_id")); assertEquals(flow.alias, start.string("desired_alias"))
            val ready = ui.pollOAuth(flow) as UiStatus.Ready
            assertEquals(AccountResult.Ok, ui.completeOAuth(flow, ready.oauthReference))
            assertEquals(UiStatus.Lost, ui.pollOAuth(flow))
            val cancelled = ui.startOAuth("p", "work", "cancel-attempt") as UiFlow.Started
            ui.cancelOAuth(cancelled)
            assertEquals("cancel-attempt", daemon.requests.last { it.string("method") == "account.oauth_cancel" }.string("attempt_id"))
            val lost = ui.startOAuth("p", "work", "lost-attempt") as UiFlow.Started
            client.close(); client.connect(RpcUiMapping.target(snapshot())!!)
            assertEquals(UiStatus.Lost, ui.pollOAuth(lost))
            assertTrue(ui.accountExists("p", "work"))
            assertFalse(ui.accountExists("p", "absent"))
        } finally { adapter.close(); source.close(); client.close(); daemon.close(); owner.cancel() }
    }

    @Test fun stagedApiKeyIsWipedEpochOwnedAndRetainsCommandAcrossRestaging() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val stages = AtomicInteger(); val attempts = AtomicInteger()
        val daemon = Daemon(owner) { body ->
            when (body.string("method")) {
                "vault.stage" -> obj("method" to "vault.stage", "vault_reference" to "synthetic-${stages.incrementAndGet()}", "expires_at_ms" to System.currentTimeMillis() + 300_000)
                "account.login_api" -> if (attempts.incrementAndGet() == 1)
                    obj("method" to "error", "code" to "provider_error", "retryable" to true) else response(body)
                else -> response(body)
            }
        }
        val client = RpcClient(owner, daemon::socket)
        val source = AccountsRepository(client, owner)
        val accounts = RpcAccountsRepository(client, source, owner)
        try {
            client.connect(RpcUiMapping.target(snapshot())!!)
            val plaintext = "synthetic-only".toCharArray()
            val first = accounts.stageApiKey(plaintext)!!
            assertTrue(plaintext.all { it == '\u0000' })
            assertFalse(daemon.requests.any { it.string("method") == "account.login_api" })
            assertEquals(AccountResult.Failed("provider_error"), accounts.commitStagedApiKey("p", "work", first, true))
            assertEquals(AccountResult.Failed("staged_secret_mismatch"), accounts.commitStagedApiKey("other", "work", first, true))
            client.close(); client.connect(RpcUiMapping.target(snapshot())!!)
            assertEquals(AccountResult.Failed("staged_secret_lost"), accounts.commitStagedApiKey("p", "work", first, true))
            assertEquals(1, attempts.get())
            val second = accounts.stageApiKey("synthetic-only".toCharArray())!!
            assertEquals(AccountResult.Ok, accounts.commitStagedApiKey("p", "work", second, true))
            val logins = daemon.requests.filter { it.string("method") == "account.login_api" }
            assertEquals(2, logins.size)
            assertEquals(logins[0].string("command_id"), logins[1].string("command_id"))
            assertEquals(second, logins[1].string("vault_reference"))
            assertEquals(JsonPrimitive(true), logins[1]["replace_existing"])
            assertEquals(AccountResult.Failed("staged_secret_lost"), accounts.commitStagedApiKey("p", "work", second, true))
            client.close()
            val offline = "synthetic-only".toCharArray()
            assertNull(accounts.stageApiKey(offline)); assertTrue(offline.all { it == '\u0000' })
        } finally { accounts.close(); source.close(); client.close(); daemon.close(); owner.cancel() }
    }

    @Test fun failedRestagedAndAbandonedKeysDoNotExhaustALongLivedEpoch() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val clock = java.util.concurrent.atomic.AtomicLong(1_000)
        val stages = AtomicInteger()
        val retryable = java.util.concurrent.atomic.AtomicBoolean()
        val succeed = java.util.concurrent.atomic.AtomicBoolean()
        val daemon = Daemon(owner) { body -> when (body.string("method")) {
            "vault.stage" -> obj("method" to "vault.stage", "vault_reference" to "synthetic-${stages.incrementAndGet()}",
                "expires_at_ms" to clock.get() + 300_000)
            "account.login_api" -> if (succeed.get()) response(body) else obj("method" to "error", "code" to "provider_error", "retryable" to retryable.get())
            else -> response(body)
        } }
        val client = RpcClient(owner, daemon::socket)
        val source = AccountsRepository(client, owner)
        val accounts = RpcAccountsRepository(client, source, owner, clock::get)
        suspend fun stage() = accounts.stageApiKey("synthetic-only".toCharArray())!!
        try {
            client.connect(RpcUiMapping.target(snapshot())!!)
            repeat(70) {
                val ref = stage()
                assertEquals(AccountResult.Failed("provider_error"), accounts.commitStagedApiKey("p", "work", ref))
                assertEquals(AccountResult.Failed("staged_secret_lost"), accounts.commitStagedApiKey("p", "work", ref))
            }
            retryable.set(true)
            var old = ""
            repeat(70) {
                old = stage()
                assertEquals(AccountResult.Failed("provider_error"), accounts.commitStagedApiKey("p", "work", old))
            }
            val retries = daemon.requests.filter { it.string("method") == "account.login_api" }.takeLast(70)
            assertEquals(1, retries.map { it.string("command_id") }.toSet().size)
            succeed.set(true)
            assertEquals(AccountResult.Ok, accounts.commitStagedApiKey("p", "work", stage()))
            assertEquals(AccountResult.Failed("staged_secret_lost"), accounts.commitStagedApiKey("p", "work", old))
            val abandoned = (1..64).map { stage() }
            assertNull(accounts.stageApiKey("synthetic-only".toCharArray()))
            clock.addAndGet(299_999)
            assertNull(accounts.stageApiKey("synthetic-only".toCharArray()))
            clock.incrementAndGet()
            val fresh = stage()
            assertEquals(AccountResult.Failed("staged_secret_lost"), accounts.commitStagedApiKey("p", "work", abandoned.first()))
            assertEquals(AccountResult.Ok, accounts.commitStagedApiKey("p", "work", fresh))
        } finally { accounts.close(); source.close(); client.close(); daemon.close(); owner.cancel() }
    }

    @Test fun menuSecretSlotsRecoverAfterTerminalAnswersAndAdvertisedExpiry() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val clock = java.util.concurrent.atomic.AtomicLong(1_000)
        val stages = AtomicInteger()
        val refuse = java.util.concurrent.atomic.AtomicBoolean(true)
        val daemon = Daemon(owner) { body -> when (body.string("method")) {
            "vault.stage" -> obj("method" to "vault.stage", "vault_reference" to "synthetic-${stages.incrementAndGet()}",
                "expires_at_ms" to clock.get() + 300_000)
            "menu.answer" -> if (refuse.get()) obj("method" to "error", "code" to "already_resolved", "retryable" to false) else response(body)
            else -> response(body)
        } }
        val client = RpcClient(owner, daemon::socket)
        val directory = Files.createTempDirectory("menu-expiry").toFile()
        val binder = object : RpcControlPlane {
            override val snapshots = MutableStateFlow<DaemonServiceSnapshot?>(snapshot())
            override suspend fun start() = Unit
            override suspend fun stop() = Unit
            override suspend fun restart() = Unit
            override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) = Unit
        }
        val service = RpcDaemonService(owner, binder, directory, "/private/workspaces", "p", "m", 4096, client, clock::get)
        val coordinates = MenuCoordinates("s", "menu", 5, 7, "menu-command")
        suspend fun stage() = service.stageMenuSecret("synthetic-only".toCharArray())
        try {
            withTimeout(3000) { service.rosterReady.first { it } }; service.activate("s")
            repeat(70) {
                val reference = stage()
                try { service.answer(coordinates, "ok", 0, MenuAnswerInput.Secret(reference)); fail() }
                catch (error: RpcRemoteException) { assertEquals("already_resolved", error.code) }
            }
            val abandoned = (1..64).map { stage() }
            try { stage(); fail("unbounded stage map") }
            catch (error: IOException) { assertEquals("staged_secret_limit", error.message) }
            clock.addAndGet(300_000)
            val fresh = stage()
            try { service.answer(coordinates, "ok", 0, MenuAnswerInput.Secret(abandoned.first())); fail() }
            catch (error: IOException) { assertEquals("staged_secret_lost", error.message) }
            val superseded = stage()
            refuse.set(false)
            service.answer(coordinates, "ok", 0, MenuAnswerInput.Secret(fresh))
            try { service.answer(coordinates, "ok", 0, MenuAnswerInput.Secret(superseded)); fail() }
            catch (error: IOException) { assertEquals("staged_secret_lost", error.message) }
        } finally { service.close(); daemon.close(); owner.cancel(); directory.deleteRecursively() }
    }

    @Test fun terminalAndExpiredOAuthFlowsReleaseSlotsAndRetryableCompletionKeepsId() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val clock = java.util.concurrent.atomic.AtomicLong(1_000)
        val flows = AtomicInteger(); val completions = AtomicInteger()
        val ready = java.util.concurrent.atomic.AtomicBoolean()
        val daemon = Daemon(owner) { body -> when (body.string("method")) {
            "account.oauth_start" -> obj("method" to "account.oauth_start", "availability" to obj("available" to true),
                "flow_id" to "synthetic-${flows.incrementAndGet()}", "expires_at_ms" to clock.get() + 300_000)
            "account.oauth_status" -> if (ready.get()) response(body) else obj("method" to "account.oauth_status", "status" to obj("status" to "expired"))
            "account.add" -> if (completions.incrementAndGet() == 1) obj("method" to "error", "code" to "provider_error", "retryable" to true) else response(body)
            else -> response(body)
        } }
        val client = RpcClient(owner, daemon::socket)
        val source = AccountsRepository(client, owner)
        val accounts = RpcAccountsRepository(client, source, owner, clock::get)
        suspend fun start() = accounts.startOAuth("p", "work", "attempt-${flows.get()}") as UiFlow.Started
        try {
            client.connect(RpcUiMapping.target(snapshot())!!)
            repeat(70) {
                val flow = start()
                assertTrue(accounts.pollOAuth(flow) is UiStatus.Failed)
                assertEquals(UiStatus.Lost, accounts.pollOAuth(flow))
            }
            val abandoned = (1..64).map { start() }
            assertTrue(accounts.startOAuth("p", "work", "overflow") is UiFlow.Unavailable)
            clock.addAndGet(300_000)
            val fresh = start()
            assertEquals(UiStatus.Lost, accounts.pollOAuth(abandoned.first()))
            ready.set(true)
            val status = accounts.pollOAuth(fresh) as UiStatus.Ready
            assertEquals(AccountResult.Failed("provider_error"), accounts.completeOAuth(fresh, status.oauthReference))
            assertEquals(AccountResult.Ok, accounts.completeOAuth(fresh, status.oauthReference))
            val adds = daemon.requests.filter { it.string("method") == "account.add" }
            assertEquals(2, adds.size); assertEquals(adds[0].string("command_id"), adds[1].string("command_id"))
            assertEquals(UiStatus.Lost, accounts.pollOAuth(fresh))
        } finally { accounts.close(); source.close(); client.close(); daemon.close(); owner.cancel() }
    }

    @Test fun responseLossRetryKeepsSemanticCommandAndOriginalCoordinates() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val attempts = AtomicInteger()
        val daemon = Daemon(owner) { body ->
            if (attempts.incrementAndGet() == 1) throw IOException("synthetic response loss after commit")
            response(body)
        }
        val client = RpcClient(owner, daemon::socket)
        val commands = RpcCommands(client)
        try {
            client.connect(RpcUiMapping.target(snapshot())!!)
            try { commands.execute("seen-s") { RpcMethods.seen(it, SessionCoordinate("s", 7)) }; fail("missing response loss") }
            catch (_: IOException) { }
            client.connect(RpcUiMapping.target(snapshot())!!)
            commands.execute("seen-s") { RpcMethods.seen(it, SessionCoordinate("s", 99)) }
            val requests = daemon.requests.toList()
            assertEquals(2, requests.size)
            assertEquals(requests[0], requests[1])
            assertEquals(7L, requests[1].number("worker_generation"))
            commands.execute("seen-s") { RpcMethods.seen(it, SessionCoordinate("s", 8)) }
            assertNotEquals(requests[1].string("command_id"), daemon.requests.last().string("command_id"))
        } finally { client.close(); daemon.close(); owner.cancel() }
    }

    @Test fun retryableRemoteAccountAndSessionErrorsKeepCommands() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val accountAttempts = AtomicInteger(); val sessionAttempts = AtomicInteger()
        val daemon = Daemon(owner) { body ->
            val method = body.string("method")
            if ((method == "account.login_api" && accountAttempts.incrementAndGet() == 1) ||
                (method == "session.seen" && sessionAttempts.incrementAndGet() == 1))
                obj("method" to "error", "code" to "provider_error", "retryable" to true)
            else response(body)
        }
        val client = RpcClient(owner, daemon::socket)
        val source = AccountsRepository(client, owner)
        val accounts = RpcAccountsRepository(client, source, owner)
        try {
            client.connect(RpcUiMapping.target(snapshot())!!)
            assertEquals(AccountResult.Failed("provider_error"), accounts.addApiKey("p", "work", "synthetic".toCharArray()))
            assertEquals(AccountResult.Ok, accounts.addApiKey("p", "work", "synthetic".toCharArray()))
            val login = daemon.requests.filter { it.string("method") == "account.login_api" }
            assertEquals(2, login.size); assertEquals(login[0].string("command_id"), login[1].string("command_id"))
            val commands = RpcCommands(client)
            try { commands.execute("seen") { RpcMethods.seen(it, SessionCoordinate("s", 7)) }; fail() }
            catch (error: RpcRemoteException) { assertTrue(error.retryable) }
            commands.execute("seen") { RpcMethods.seen(it, SessionCoordinate("s", 99)) }
            val seen = daemon.requests.filter { it.string("method") == "session.seen" }
            assertEquals(seen[0], seen[1])
        } finally { accounts.close(); source.close(); client.close(); daemon.close(); owner.cancel() }
    }

    @Test fun coldRosterDoesNotPublishRunningOrCreateFromProvisionalEmptyCache() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val listRequested = CompletableDeferred<Unit>()
        val release = java.util.concurrent.CountDownLatch(1)
        val daemon = Daemon(owner) { body ->
            if (body.string("method") == "session.list") {
                listRequested.complete(Unit)
                check(release.await(5, java.util.concurrent.TimeUnit.SECONDS))
            }
            response(body)
        }
        val client = RpcClient(owner, daemon::socket)
        val directory = Files.createTempDirectory("cold-roster").toFile()
        val binder = object : RpcControlPlane {
            override val snapshots = MutableStateFlow<DaemonServiceSnapshot?>(snapshot())
            override suspend fun start() = Unit
            override suspend fun stop() = Unit
            override suspend fun restart() = Unit
            override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) = Unit
        }
        val service = RpcDaemonService(owner, binder, directory, "/private/workspaces", "p", "m", 4096, client)
        try {
            withTimeout(3000) { listRequested.await() }
            assertFalse(service.rosterReady.value)
            assertEquals(DaemonStatus.Starting, withTimeout(3000) { service.status.first { it == DaemonStatus.Starting } })
            try { service.createSession(); fail("provisional empty cache was used as create authority") }
            catch (error: IOException) { assertEquals("roster_not_ready", error.message) }
            release.countDown()
            withTimeout(3000) { service.status.first { it is DaemonStatus.Running } }
            assertEquals(2, service.sessions.value.size)
            assertFalse(daemon.requests.any { it.string("method") == "session.create" })
            service.search("needle")
            withTimeout(3000) { service.searchIndex.first { it.complete } }
            client.close()
            withTimeout(3000) { service.searchIndex.first { !it.complete } }
        } finally { release.countDown(); service.close(); daemon.close(); owner.cancel(); directory.deleteRecursively() }
    }

    @Test fun delayedPushesUpdateTranscriptStreamWithoutAnotherUiActionOrReattach() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val daemon = Daemon(owner, response = ::response)
        val client = RpcClient(owner, daemon::socket)
        val directory = Files.createTempDirectory("live-transcript").toFile()
        val binder = object : RpcControlPlane {
            override val snapshots = MutableStateFlow<DaemonServiceSnapshot?>(snapshot())
            override suspend fun start() = Unit
            override suspend fun stop() = Unit
            override suspend fun restart() = Unit
            override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) = Unit
        }
        val service = RpcDaemonService(owner, binder, directory, "/private/workspaces", "p", "m", 4096, client)
        try {
            withTimeout(3000) { service.rosterReady.first { it } }
            service.activate("s")
            val rendered = service.transcriptUpdates("s").stateIn(owner, SharingStarted.Eagerly, TranscriptLoad.Unavailable("pending"))
            withTimeout(3000) { rendered.first { it is TranscriptLoad.Complete } }
            service.send("s", "next")
            val calls = daemon.requests.size
            fun push(seq: Long, payload: JsonObject) = daemon.push(obj("v" to 1, "kind" to "event", "attachment_id" to "attachment-s", "session_id" to "s",
                "envelope" to obj("seq" to seq, "session_id" to "s", "render" to obj("ui" to true), "payload" to payload)))
            delay(50)
            push(3, obj("type" to "item", "event" to "delta", "item_id" to "live", "delta" to obj("delta" to "text", "text" to "streamed")))
            withTimeout(3000) { rendered.first { it.messages.lastOrNull()?.text == "streamed" } }
            assertTrue(rendered.value.messages.last().streaming)
            push(4, obj("type" to "item", "event" to "completed", "item_id" to "live", "item" to obj("item" to "agent_message", "text" to "final after submit")))
            withTimeout(3000) { rendered.first { it.messages.lastOrNull()?.text == "final after submit" } }
            assertFalse(rendered.value.messages.last().streaming)
            assertEquals(calls, daemon.requests.size)
        } finally { service.close(); daemon.close(); owner.cancel(); directory.deleteRecursively() }
    }

    @Test fun controlLeaseBlocksConcurrentActivationUntilProtectedRequestCompletes() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val release = CompletableDeferred<Unit>()
        val daemon = Daemon(owner, "session.rename", release, response = ::response)
        val client = RpcClient(owner, daemon::socket)
        val directory = Files.createTempDirectory("control-lease").toFile()
        val binder = object : RpcControlPlane {
            override val snapshots = MutableStateFlow<DaemonServiceSnapshot?>(snapshot())
            override suspend fun start() = Unit
            override suspend fun stop() = Unit
            override suspend fun restart() = Unit
            override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) = Unit
        }
        val service = RpcDaemonService(owner, binder, directory, "/private/workspaces", "p", "m", 4096, client)
        try {
            withTimeout(3000) { service.rosterReady.first { it } }
            service.activate("s")
            val protected = async { service.rename("s", "renamed") }
            withTimeout(3000) { while (daemon.requests.none { it.string("method") == "session.rename" }) delay(5) }
            val beforeSwitch = daemon.requests.size
            val switching = async(start = CoroutineStart.UNDISPATCHED) { service.activate("child") }
            delay(50)
            assertFalse(switching.isCompleted)
            assertFalse(daemon.requests.drop(beforeSwitch).any { it.string("method") in setOf("session.attach", "session.detach") })
            release.complete(Unit)
            withTimeout(3000) { protected.await(); switching.await() }
            assertEquals("child", service.activeSessionId.value)
            assertTrue(daemon.requests.drop(beforeSwitch).any { it.string("method") == "session.detach" })
        } finally { release.complete(Unit); service.close(); daemon.close(); owner.cancel(); directory.deleteRecursively() }
    }

    @Test fun mappingsPreserveUnknownsAndAllRosterPresentationFacts() {
        val minimal = RpcUiMapping.row(SessionSummary.parse(obj("session_id" to "s", "head_seq" to 0, "worker_generation" to 1)))
        assertNull(minimal.runState); assertNull(minimal.fast); assertNull(minimal.createdAtMs)
        assertNull(minimal.provider); assertNull(minimal.footprintTokens); assertEquals(SessionVisualState.Unknown, minimal.state)
        val full = JsonObject(summary() + obj("metadata" to obj("created_at_ms" to 44), "agent_type" to "reviewer",
            "last_activity_ms" to 50, "seen_at_ms" to 49, "turn_count" to 3, "footprint_tokens" to 0, "footprint_truth" to "exact",
            "workspace_cwd" to "/workspace", "forked_from" to obj("session_id" to "original", "seq" to 4), "parent_session_id" to "parent", "kind" to "subagent"))
        val row = RpcUiMapping.row(SessionSummary.parse(full))
        assertEquals(44L, row.createdAtMs); assertEquals("reviewer", row.agentType); assertEquals(50L, row.lastActivityMs)
        assertEquals(49L, row.seenAtMs); assertEquals(3L, row.turnCount); assertEquals(0L, row.footprintTokens); assertEquals(true, row.footprintExact)
        assertEquals("/workspace", row.workspaceCwd); assertEquals(ForkProvenance("original", 4), row.forkedFrom)
        assertEquals("parent", row.parentSessionId); assertEquals("subagent", row.kind); assertEquals("secret", row.needsInput!!.kind)
        assertEquals(SessionVisualState.NeedsInput, row.state)
        assertEquals(AuthKind.Unknown, RpcAccountsRepository.account(Account("a", "p", null, "future", false, null, "unknown")).authKind)
        assertTrue(RpcAccountsRepository.status(OAuthStatus("future", null, null)) is UiStatus.Failed)
        for (state in RpcConnectionState.entries) assertEquals(state.ordinal, RpcUiMapping.dataPlane(state).ordinal)
        assertEquals("0.0.970", RpcUiMapping.target(snapshot())!!.appVersion)
        assertNull(RpcUiMapping.target(snapshot().copy(phase = DaemonPhase.Stopping)))
        assertThrows(RpcProtocolException::class.java) { RpcUiMapping.target(snapshot().copy(daemonGeneration = 99)) }
    }

    /**
     * The 971-V UI findings, as one seam test over the real facade.
     *
     * Every assertion here is a door the production adapter did not have: Auto
     * standing consent (F3), the custom-server probe/configure pair (F9), the
     * inventory authority the picker gates free-text model entry on (F6), the
     * Loom capability read from the actual Hello (F8), and re-observation after
     * a reconnect (F4).
     */
    @Test fun productionAdapterAnswersStandingConsentCustomServersAndTheLoomDoor() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        // A device-capability approval, exactly as the daemon publishes one:
        // one covered tool name and a decision-tagged option.
        val parked = obj("session_id" to "s", "head_seq" to 2, "worker_generation" to 7,
            "provider" to "p", "last_model" to "m", "run_state" to "parked_permission", "run_id" to "run",
            "needs_input" to obj("kind" to "permission", "title" to "Allow sms.list for the last 20 messages?",
                "safe_body" to JsonArray(listOf(JsonPrimitive("The agent asked to read your recent texts."))),
                "menu_id" to "menu-sms", "request_seq" to 5, "worker_generation" to 7, "secret_answer" to false,
                "options" to JsonArray(listOf(
                    obj("key" to "allow", "label" to "Allow once", "decision" to "allow_once"),
                    obj("key" to "reject", "label" to "Don't allow", "decision" to "reject_once")))))
        val daemon = Daemon(owner) { body ->
            when (body.string("method")) {
                "session.list" -> obj("method" to "session.list", "sessions" to JsonArray(listOf(parked)))
                else -> response(body)
            }
        }
        val client = RpcClient(owner, daemon::socket)
        val directory = Files.createTempDirectory("vfix").toFile()
        val binder = object : RpcControlPlane {
            override val snapshots = MutableStateFlow<DaemonServiceSnapshot?>(snapshot())
            override suspend fun start() = Unit
            override suspend fun stop() = Unit
            override suspend fun restart() = Unit
            override suspend fun reportNotificationPermission(granted: Boolean, permanentlyDenied: Boolean) = Unit
        }
        val service = RpcDaemonService(owner, binder, directory, "/private/workspaces", "p", "m", 4096, client)
        val ui: DaemonService = service
        try {
            withTimeout(5000) { ui.sessions.first { it.size == 1 } }
            ui.activate("s")
            withTimeout(5000) { ui.models.first { it != null } }

            // F6: the advisory authority the daemon published survives all the
            // way to the option the picker reads.
            assertEquals("advisory", ui.providers.value.providers.single().inventoryAuthority)
            assertEquals("advisory", ui.models.value!!.providers.single().inventoryAuthority)

            // F8: `loom_v1` is in the actual Hello, so the screen gets a
            // registry rather than the hardcoded unavailable reason.
            assertNull(ui.loomUnavailable())
            assertNotNull(ui.loomList(false))
            assertTrue(daemon.requests.any { it.string("method") == "loom.list" })

            // F3: switching to Auto answers the covered card with its
            // `allow_once` option, using the coordinates the card carried.
            ui.setPermissionMode(PermissionMode.Auto)
            val answered = withTimeout(5000) {
                var found: JsonObject? = null
                while (found == null) {
                    found = daemon.requests.lastOrNull { it.string("method") == "menu.answer" }
                    if (found == null) delay(20)
                }
                found
            }
            assertEquals("menu-sms", answered.string("menu_id"))
            assertEquals("allow", answered.string("option_key"))
            assertEquals(0L, answered.number("option_index"))
            assertEquals(5L, answered.number("request_seq"))
            assertEquals(7L, answered.number("worker_generation"))
            // Consent is spent once per menu, however often the roster re-emits.
            delay(120)
            assertEquals(1, daemon.requests.count { it.string("method") == "menu.answer" })

            // F4: a restarted daemon reconnects on the same socket path, so the
            // Binder target never changes. Re-observation has to come from the
            // connection epoch instead.
            val attachmentsBefore = daemon.requests.count { it.string("method") == "session.attach" }
            client.close()
            withTimeout(5000) { client.state.first { it != RpcConnectionState.CONNECTED } }
            client.connect(RpcUiMapping.target(snapshot())!!)
            withTimeout(5000) {
                while (daemon.requests.count { it.string("method") == "session.attach" } <= attachmentsBefore) delay(20)
            }
            assertTrue(daemon.requests.count { it.string("method") == "session.list" } > 1)
        } finally { service.close(); daemon.close(); owner.cancel(); directory.deleteRecursively() }
    }

    /** F9: the custom-server card's two doors, on the production adapter. */
    @Test fun customServerProbesAndConfiguresThroughTheProductionAdapter() = runBlocking<Unit> {
        val owner = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val daemon = Daemon(owner, response = ::response)
        val client = RpcClient(owner, daemon::socket)
        val source = AccountsRepository(client, owner)
        val adapter = RpcAccountsRepository(client, source, owner)
        val ui: ai.diffforge.haider.ui.accounts.AccountsRepository = adapter
        try {
            client.connect(RpcUiMapping.target(snapshot())!!)
            ui.refreshProviders()
            withTimeout(3000) { ui.providers.first { it.isNotEmpty() } }
            // Round 11 left the interface defaults in place, so the real form
            // answered `provider_models_probe_v1` / `provider_configure_v1`
            // while the daemon advertised both (971-V F9).
            val probe = ui.probeCustomModels("local", "http://127.0.0.1:1", "openai_chat_completions", keyless = true)
            assertEquals(CustomModelsProbe.Models(listOf("probe-b", "probe-a"), "probe-b"), probe)
            val request = daemon.requests.last { it.string("method") == "provider.models_probe" }
            // Read-only discovery: no command id, because it is not a durable mutation.
            assertFalse(request.containsKey("command_id"))
            assertEquals(JsonPrimitive(true), request["keyless"])
            assertFalse(request.containsKey("probe_vault_reference"))

            assertEquals(AccountResult.Ok, ui.configureCustomProvider("local", "http://127.0.0.1:1",
                "openai_chat_completions", "none", listOf("probe-b"), "probe-b", expectedRevision = 12))
            val configure = daemon.requests.last { it.string("method") == "provider.configure" }
            assertEquals(12L, configure.number("expected_revision"))
            assertEquals(JsonPrimitive(true), configure["enabled"])
            // The stage is spent by `account.login_api`, never here.
            assertFalse(configure.containsKey("probe_vault_reference"))
            // F6: the authority the summary stated reaches the UI descriptor.
            assertEquals(ModelInventoryAuthority.Advisory, ui.providers.value.single().inventoryAuthority)
        } finally { adapter.close(); source.close(); client.close(); daemon.close(); owner.cancel() }
    }
}
