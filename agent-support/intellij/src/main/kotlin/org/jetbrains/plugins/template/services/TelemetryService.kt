package org.jetbrains.plugins.template.services

import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationInfo
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.diagnostic.thisLogger
import com.posthog.java.HttpSender
import com.posthog.java.PostHog as PostHogClient
import com.posthog.java.PostHogLogger
import com.posthog.java.QueueManager
import com.posthog.java.Sender
import com.posthog.java.shaded.org.json.JSONObject
import io.sentry.Hint
import io.sentry.Sentry
import io.sentry.SentryEvent
import io.sentry.SentryLevel
import io.sentry.SentryOptions
import io.sentry.protocol.Message
import java.io.File

/**
 * Application-level service for analytics (PostHog) and error reporting (Sentry).
 * Matches the VS Code extension telemetry pattern.
 */
@Service(Service.Level.APP)
class TelemetryService : Disposable {

    private val logger = thisLogger()

    private var posthog: PostHogClient? = null
    private var distinctId: String? = null
    private var sentryInitialized = false

    companion object {
        private const val POSTHOG_API_KEY = "phc_XANaHNpDXBERPosyM8Bp0INVoGsgW8Gk92HsB090r6A"
        private const val POSTHOG_HOST = "https://us.i.posthog.com"
        private const val SENTRY_DSN = "https://8316e787580a70ad21e7158027caf849@o4510273204649984.ingest.us.sentry.io/4510812879060992"
        private const val PLUGIN_ID = "com.usegitai.plugins.jetbrains"

        /**
         * Package prefixes used to identify events originating from our plugin.
         * Events with stack frames matching these prefixes are sent to Sentry;
         * all other JVM exceptions (e.g. JetBrains internal errors) are dropped.
         */
        private val PLUGIN_PACKAGE_PREFIXES = listOf(
            "org.jetbrains.plugins.template",
            "com.usegitai"
        )

        /** Tag key set on events we create directly (not from uncaught exceptions). */
        private const val PLUGIN_EVENT_TAG = "git_ai_plugin_event"

        fun getInstance(): TelemetryService = service()

        /**
         * Returns the TelemetryService instance or null if service instantiation fails.
         * Use this from call sites where telemetry failures must never propagate.
         */
        fun getInstanceOrNull(): TelemetryService? {
            return try {
                service()
            } catch (_: Throwable) {
                null
            }
        }
    }

    init {
        if (!isOssTelemetryDisabled()) {
            try {
                initializePostHog()
            } catch (_: Throwable) {
                // Silently ignore – telemetry must never surface errors to the user
            }
            try {
                initializeSentry()
            } catch (_: Throwable) {
                // Silently ignore – telemetry must never surface errors to the user
            }
        } else {
            logger.info("OSS telemetry disabled by user config")
        }
    }

    private fun initializePostHog() {
        try {
            distinctId = readDistinctId()
            if (distinctId == null) {
                logger.info("No distinct_id found, PostHog analytics disabled")
                return
            }

            // Silence PostHog's JUL logger as a safety net — prevents any
            // SEVERE-level log from reaching IntelliJ's error dialog bridge.
            silencePostHogJulLogger()

            val httpSender = HttpSender.Builder(POSTHOG_API_KEY)
                .host(POSTHOG_HOST)
                .logger(SilentPostHogLogger)
                .build()
            val safeSender = SafeSender(httpSender)
            val queueManager = QueueManager.Builder(safeSender).build()

            posthog = PostHogClient.BuilderWithCustomQueueManager(queueManager, safeSender)
                .build()

            logger.info("PostHog initialized with distinct_id: $distinctId")
        } catch (e: Exception) {
            logger.warn("Failed to initialize PostHog: ${e.message}")
        }
    }

    /**
     * No-op PostHogLogger that replaces DefaultPostHogLogger to prevent
     * PostHog's internal error logging from triggering IntelliJ's IDE
     * Internal Errors dialog.
     *
     * Root cause: HttpSender.send() catches IOException internally and logs
     * via DefaultPostHogLogger → java.util.logging.Logger.log(Level.SEVERE, ...)
     * IntelliJ bridges JUL SEVERE to Logger.error() which shows the error
     * dialog. The SafeSender wrapper never sees these exceptions because
     * they are caught and logged inside HttpSender before reaching it.
     */
    private object SilentPostHogLogger : PostHogLogger {
        override fun debug(message: String) {}
        override fun info(message: String) {}
        override fun warn(message: String) {}
        override fun error(message: String) {}
        override fun error(message: String, throwable: Throwable) {}
    }

    /**
     * Silences the JUL logger used by PostHog's DefaultPostHogLogger as
     * a belt-and-suspenders safety net alongside SilentPostHogLogger.
     */
    private fun silencePostHogJulLogger() {
        try {
            java.util.logging.Logger.getLogger("com.posthog.java.PostHog").level =
                java.util.logging.Level.OFF
        } catch (_: Throwable) {
            // Best effort — ignore if JUL manipulation fails
        }
    }

    /**
     * Wraps a PostHog Sender to prevent network exceptions from propagating
     * as uncaught exceptions on the QueueManager background thread.
     */
    private class SafeSender(private val delegate: Sender) : Sender {
        private val logger = com.intellij.openapi.diagnostic.Logger.getInstance(SafeSender::class.java)

        override fun send(messages: List<JSONObject>): Boolean? {
            return try {
                delegate.send(messages)
            } catch (e: Exception) {
                logger.info("PostHog send failed (non-critical): ${e.javaClass.simpleName}")
                null
            }
        }

        override fun post(url: String, body: String): JSONObject? {
            return try {
                delegate.post(url, body)
            } catch (e: Exception) {
                logger.info("PostHog post failed (non-critical): ${e.javaClass.simpleName}")
                null
            }
        }
    }

    private fun initializeSentry() {
        try {
            Sentry.init { options ->
                options.dsn = SENTRY_DSN
                options.release = pluginVersion
                options.environment = "production"
                options.tracesSampleRate = 0.3
                options.setTag("ide", "intellij")
                options.setTag("ide_version", ApplicationInfo.getInstance().fullVersion)

                // Filter out events that don't originate from our plugin.
                // Since the plugin runs in the same JVM as the IDE, Sentry would
                // otherwise capture JetBrains internal errors as well.
                options.beforeSend = SentryOptions.BeforeSendCallback { event: SentryEvent, _: Hint ->
                    if (isPluginEvent(event)) event else null
                }
            }
            sentryInitialized = true
            logger.info("Sentry initialized")
        } catch (e: Exception) {
            logger.warn("Failed to initialize Sentry: ${e.message}")
        }
    }

    /**
     * Returns true if the event was explicitly sent by our plugin (tagged) or if
     * any exception in the event has a stack frame from our plugin packages.
     */
    private fun isPluginEvent(event: SentryEvent): Boolean {
        // Events we send ourselves are tagged
        if (event.getTag(PLUGIN_EVENT_TAG) != null) return true

        // Check exception stack traces for our package prefixes
        val exceptions = event.exceptions ?: return false
        return exceptions.any { exception ->
            exception.stacktrace?.frames?.any { frame ->
                val module = frame.module ?: return@any false
                PLUGIN_PACKAGE_PREFIXES.any { prefix -> module.startsWith(prefix) }
            } ?: false
        }
    }

    private fun isOssTelemetryDisabled(): Boolean {
        return true
    }

    private fun readDistinctId(): String? {
        return try {
            val homeDir = System.getProperty("user.home")
            val distinctIdFile = File(homeDir, ".git-ai/internal/distinct_id")
            if (distinctIdFile.exists()) {
                distinctIdFile.readText().trim().takeIf { it.isNotEmpty() }
            } else {
                null
            }
        } catch (e: Exception) {
            logger.warn("Failed to read distinct_id: ${e.message}")
            null
        }
    }

    private val pluginVersion: String by lazy {
        try {
            val xml = this::class.java.classLoader
                .getResourceAsStream("META-INF/plugin.xml")
                ?.bufferedReader()?.use { it.readText() } ?: return@lazy "unknown"
            Regex("<version>(.+?)</version>").find(xml)
                ?.groupValues?.get(1) ?: "unknown"
        } catch (_: Exception) {
            "unknown"
        }
    }

    private fun getCommonProperties(): Map<String, Any> {
        return mapOf(
            "ide" to "intellij",
            "ide_version" to ApplicationInfo.getInstance().fullVersion,
            "ide_build" to ApplicationInfo.getInstance().build.asString(),
            "plugin_version" to pluginVersion,
            "os" to (System.getProperty("os.name") ?: "unknown"),
            "arch" to (System.getProperty("os.arch") ?: "unknown")
        )
    }

    /**
     * Captures the plugin startup event.
     */
    fun captureStartupEvent() {
        try {
            captureEvent("intellij_plugin_startup", getCommonProperties())
        } catch (_: Throwable) {
            // Non-critical – never surface to user
        }
    }

    /**
     * Reports that git-ai CLI was not found (simple version for backwards compatibility).
     */
    fun reportGitAiNotFound() {
        reportGitAiNotFound(null, null, emptyList(), null)
    }

    /**
     * Reports that git-ai CLI was not found with detailed context.
     */
    fun reportGitAiNotFound(
        exitCode: Int?,
        output: String?,
        searchedPaths: List<String>,
        currentPath: String?
    ) {
        try {
            val context = mutableMapOf<String, Any>(
                "error_type" to "git_ai_not_found"
            )
            exitCode?.let { context["exit_code"] = it }
            output?.let { context["output"] = it.take(500) }
            if (searchedPaths.isNotEmpty()) {
                context["searched_paths"] = searchedPaths.joinToString(",")
            }
            currentPath?.let { context["path_env"] = it.take(500) }

            captureEvent("git_ai_error", getCommonProperties() + context)

            val message = buildString {
                append("git-ai CLI not found")
                exitCode?.let { append(" (exit code: $it)") }
                if (searchedPaths.isNotEmpty()) {
                    append(". Searched: ${searchedPaths.joinToString(", ")}")
                }
            }
            captureSentryMessage(message, SentryLevel.WARNING, context)
        } catch (_: Throwable) {
            // Non-critical – never surface to user
        }
    }

    /**
     * Reports a version mismatch where git-ai is below minimum required version.
     */
    fun reportVersionMismatch(foundVersion: String, requiredVersion: String) {
        try {
            val context = mapOf(
                "error_type" to "version_mismatch",
                "found_version" to foundVersion,
                "required_version" to requiredVersion
            )
            captureEvent("git_ai_error", getCommonProperties() + context)
            captureSentryMessage(
                "git-ai version mismatch: found $foundVersion, required $requiredVersion",
                SentryLevel.WARNING,
                context
            )
        } catch (_: Throwable) {
            // Non-critical – never surface to user
        }
    }

    /**
     * Reports a checkpoint failure.
     */
    fun reportCheckpointFailure(exitCode: Int, output: String) {
        try {
            val context = mapOf(
                "error_type" to "checkpoint_failure",
                "exit_code" to exitCode.toString(),
                "output" to output.take(500) // Limit output size
            )
            captureEvent("git_ai_error", getCommonProperties() + context)
            captureSentryMessage(
                "git-ai checkpoint failed with exit code $exitCode",
                SentryLevel.ERROR,
                context
            )
        } catch (_: Throwable) {
            // Non-critical – never surface to user
        }
    }

    /**
     * Reports a checkpoint timeout (exceeded 30s).
     */
    fun reportCheckpointTimeout() {
        try {
            val context = mapOf("error_type" to "checkpoint_timeout")
            captureEvent("git_ai_error", getCommonProperties() + context)
            captureSentryMessage("git-ai checkpoint timed out after 30 seconds", SentryLevel.ERROR, context)
        } catch (_: Throwable) {
            // Non-critical – never surface to user
        }
    }

    /**
     * Captures a general error/exception.
     */
    fun captureError(throwable: Throwable, context: Map<String, String> = emptyMap()) {
        try {
            val eventContext = mapOf("error_type" to "exception") + context
            captureEvent("git_ai_error", getCommonProperties() + eventContext)

            if (sentryInitialized) {
                Sentry.captureException(throwable) { scope ->
                    scope.setTag(PLUGIN_EVENT_TAG, "true")
                    context.forEach { (key, value) ->
                        scope.setExtra(key, value)
                    }
                }
            }
        } catch (_: Throwable) {
            // Non-critical – never surface to user
        }
    }

    private fun captureEvent(eventName: String, properties: Map<String, Any>) {
        val ph = posthog ?: return
        val id = distinctId ?: return

        try {
            ph.capture(id, eventName, properties)
        } catch (e: Exception) {
            logger.warn("Failed to capture PostHog event: ${e.message}")
        }
    }

    private fun captureSentryMessage(message: String, level: SentryLevel, context: Map<String, Any>) {
        if (!sentryInitialized) return

        try {
            val event = SentryEvent().apply {
                this.message = Message().apply { this.message = message }
                this.level = level
                setTag(PLUGIN_EVENT_TAG, "true")
                context.forEach { (key, value) ->
                    setExtra(key, value)
                }
                // Add distinct_id if available
                distinctId?.let { setExtra("distinct_id", it) }
            }
            Sentry.captureEvent(event)
        } catch (e: Exception) {
            logger.warn("Failed to capture Sentry message: ${e.message}")
        }
    }

    override fun dispose() {
        try {
            posthog?.shutdown()
        } catch (_: Throwable) {
            // Silently ignore – shutdown errors are non-critical
        }

        try {
            if (sentryInitialized) {
                Sentry.close()
            }
        } catch (_: Throwable) {
            // Silently ignore – shutdown errors are non-critical
        }
    }
}
