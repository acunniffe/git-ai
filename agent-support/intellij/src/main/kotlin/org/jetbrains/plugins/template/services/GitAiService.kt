package org.jetbrains.plugins.template.services

import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.diagnostic.thisLogger
import org.jetbrains.plugins.template.model.AgentV1Input
import org.jetbrains.plugins.template.model.KnownHumanInput
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Application-level service that interacts with the git-ai CLI
 * to create checkpoints when AI agents make edits.
 */
@Service(Service.Level.APP)
class GitAiService {

    private val logger = thisLogger()
    private val minVersion = Version(1, 0, 23)

    // Stable session ID based on when the service was initialized
    val sessionId: String = System.currentTimeMillis().toString()

    @Volatile
    private var availabilityChecked = false

    @Volatile
    private var isAvailable = false

    @Volatile
    private var cachedVersion: Version? = null

    // Cached path to git-ai binary once resolved
    @Volatile
    private var resolvedGitAiPath: String? = null

    // Track which locations were searched (for error reporting)
    private var lastSearchedPaths: List<String> = emptyList()

    private val gitAiBinaryResolver = GitAiBinaryResolver()

    data class Version(val major: Int, val minor: Int, val patch: Int) : Comparable<Version> {
        override fun compareTo(other: Version): Int {
            return compareValuesBy(this, other, { it.major }, { it.minor }, { it.patch })
        }

        override fun toString(): String = "$major.$minor.$patch"

        companion object {
            fun parse(versionString: String): Version? {
                // Expected format: "1.0.39 (debug)" or "1.0.39"
                val versionPart = versionString.trim().split(" ").first()

                val parts = versionPart.split(".")
                if (parts.size < 3) return null

                return try {
                    Version(
                        parts[0].toInt(),
                        parts[1].toInt(),
                        parts[2].split("-", "+").first().toInt()
                    )
                } catch (e: NumberFormatException) {
                    null
                }
            }
        }
    }

    /** Finds git-ai using the resolver's cache, known locations, and IntelliJ's console PATH. */
    private fun findGitAiBinary(): String? {
        val previousPath = resolvedGitAiPath
        val resolution = gitAiBinaryResolver.resolve(previousPath)
        lastSearchedPaths = resolution.searchedPaths

        val path = resolution.executablePath
        if (path == null) {
            resolvedGitAiPath = null
            return null
        }

        if (path != previousPath) {
            logger.info("Found git-ai executable at: $path")
        }
        resolvedGitAiPath = path
        return path
    }

    /**
     * Checks if git-ai CLI is installed and meets the minimum version requirement.
     */
    fun checkAvailable(): Boolean {
        if (availabilityChecked) {
            return isAvailable
        }

        synchronized(this) {
            if (availabilityChecked) {
                return isAvailable
            }

            isAvailable = checkGitAiInstalled()
            availabilityChecked = true
            return isAvailable
        }
    }

    private fun checkGitAiInstalled(): Boolean {
        return try {
            // First, try to find the git-ai binary
            val gitAiPath = findGitAiBinary()

            if (gitAiPath == null) {
                val currentPath = currentPathForDiagnostics()
                logger.warn("""
                    git-ai not found
                    Searched locations: ${lastSearchedPaths.joinToString(", ")}
                    PATH: $currentPath

                    To fix: Install git-ai using one of these methods:
                    - cargo install git-ai
                    - curl -fsSL https://install.usegitai.com | sh
                    - Or ensure git-ai is in your PATH
                """.trimIndent())
                TelemetryService.getInstanceOrNull()?.reportGitAiNotFound(
                    exitCode = null,
                    output = null,
                    searchedPaths = lastSearchedPaths,
                    currentPath = currentPath
                )
                return false
            }

            // Run version check using the resolved path
            val command = listOf(gitAiPath, "version")
            val process = ProcessBuilder(command)
                .redirectErrorStream(false)
                .start()

            val completed = process.waitFor(5, TimeUnit.SECONDS)
            if (!completed) {
                process.destroyForcibly()
                logger.warn("git-ai version check timed out")
                return false
            }

            val output = process.inputStream.bufferedReader().readText().trim()
            val errorOutput = process.errorStream.bufferedReader().readText().trim()

            if (process.exitValue() != 0) {
                val currentPath = currentPathForDiagnostics()
                logger.warn("""
                    git-ai returned error
                    Command: ${command.joinToString(" ")}
                    Exit code: ${process.exitValue()}
                    Stdout: $output
                    Stderr: $errorOutput
                    PATH: $currentPath
                    Resolved path: $gitAiPath
                """.trimIndent())
                TelemetryService.getInstanceOrNull()?.reportGitAiNotFound(
                    exitCode = process.exitValue(),
                    output = if (errorOutput.isNotEmpty()) errorOutput else output,
                    searchedPaths = lastSearchedPaths,
                    currentPath = currentPath
                )
                return false
            }

            val version = Version.parse(output)

            if (version == null) {
                logger.warn("Could not parse git-ai version from: $output")
                return false
            }

            cachedVersion = version

            if (version < minVersion) {
                logger.warn("git-ai version $version is below minimum required version $minVersion")
                TelemetryService.getInstanceOrNull()?.reportVersionMismatch(version.toString(), minVersion.toString())
                return false
            }

            logger.info("git-ai CLI available at $gitAiPath, version: $version")
            true
        } catch (e: Exception) {
            val currentPath = currentPathForDiagnostics()
            logger.warn("""
                git-ai CLI not available: ${e.message}
                Searched locations: ${lastSearchedPaths.joinToString(", ")}
                PATH: $currentPath
            """.trimIndent(), e)
            TelemetryService.getInstanceOrNull()?.captureError(e, mapOf(
                "context" to "git_ai_availability_check",
                "searched_paths" to lastSearchedPaths.joinToString(","),
                "current_path" to currentPath
            ))
            false
        }
    }

    private fun currentPathForDiagnostics(): String =
        "IntelliJ console PATH searched (value redacted)"

    /**
     * Creates a checkpoint by calling git-ai checkpoint agent-v1 command.
     *
     * @param input The checkpoint data to send via stdin (Human or AiAgent)
     * @param workingDirectory The working directory (git repo root) for the command
     * @return true if checkpoint was created successfully
     */
    fun checkpoint(input: AgentV1Input, workingDirectory: String): Boolean {
        if (!checkAvailable()) {
            logger.warn("Skipping checkpoint - git-ai not available")
            return false
        }

        val gitAiPath = resolvedGitAiPath
        if (gitAiPath == null) {
            logger.warn("Skipping checkpoint - git-ai path not resolved")
            return false
        }

        return try {
            val jsonInput = input.toJson()
            val inputType = when (input) {
                is AgentV1Input.Human -> "human"
                is AgentV1Input.AiAgent -> "ai_agent (${input.agentName})"
            }

            logger.info("Creating checkpoint (agent-v1): $inputType")
            logger.info("Checkpoint input: $jsonInput")

            val command = listOf(gitAiPath, "checkpoint", "agent-v1", "--hook-input", "stdin")
            val process = ProcessBuilder(command)
                .directory(File(workingDirectory))
                .redirectErrorStream(false)
                .start()

            // Write JSON to stdin
            process.outputStream.bufferedWriter().use { writer ->
                writer.write(jsonInput)
            }

            val completed = process.waitFor(30, TimeUnit.SECONDS)
            if (!completed) {
                process.destroyForcibly()
                logger.warn("git-ai checkpoint timed out")
                TelemetryService.getInstanceOrNull()?.reportCheckpointTimeout()
                return false
            }

            val output = process.inputStream.bufferedReader().readText().trim()
            val errorOutput = process.errorStream.bufferedReader().readText().trim()
            val exitCode = process.exitValue()

            if (exitCode != 0) {
                val combinedOutput = if (errorOutput.isNotEmpty()) "$output\n$errorOutput" else output
                logger.warn("""
                    git-ai checkpoint failed
                    Command: ${command.joinToString(" ")}
                    Exit code: $exitCode
                    Stdout: $output
                    Stderr: $errorOutput
                """.trimIndent())
                TelemetryService.getInstanceOrNull()?.reportCheckpointFailure(exitCode, combinedOutput)
                return false
            }

            logger.info("Checkpoint created successfully ($inputType)")
            if (output.isNotEmpty()) {
                logger.info("git-ai output: $output")
            }
            true
        } catch (e: Exception) {
            logger.warn("Failed to create checkpoint: ${e.message}", e)
            TelemetryService.getInstanceOrNull()?.captureError(e, mapOf("context" to "checkpoint_creation"))
            false
        }
    }

    /**
     * Creates a known_human checkpoint by calling git-ai checkpoint known_human command.
     *
     * @param input The checkpoint data to send via stdin
     * @param workingDirectory The working directory (git repo root) for the command
     * @return true if checkpoint was created successfully
     */
    fun checkpointKnownHuman(input: KnownHumanInput, workingDirectory: String): Boolean {
        if (!checkAvailable()) {
            logger.warn("Skipping known_human checkpoint - git-ai not available")
            return false
        }

        val gitAiPath = resolvedGitAiPath
        if (gitAiPath == null) {
            logger.warn("Skipping known_human checkpoint - git-ai path not resolved")
            return false
        }

        return try {
            val jsonInput = input.toJson()
            logger.info("Creating known_human checkpoint for ${input.editedFilepaths}")
            logger.info("known_human checkpoint input: $jsonInput")

            val command = listOf(gitAiPath, "checkpoint", "known_human", "--hook-input", "stdin")
            val process = ProcessBuilder(command)
                .directory(File(workingDirectory))
                .redirectErrorStream(false)
                .start()

            process.outputStream.bufferedWriter().use { writer ->
                writer.write(jsonInput)
            }

            val completed = process.waitFor(30, TimeUnit.SECONDS)
            if (!completed) {
                process.destroyForcibly()
                logger.warn("git-ai known_human checkpoint timed out")
                return false
            }

            val output = process.inputStream.bufferedReader().readText().trim()
            val errorOutput = process.errorStream.bufferedReader().readText().trim()
            val exitCode = process.exitValue()

            if (exitCode != 0) {
                logger.warn("""
                    git-ai known_human checkpoint failed
                    Command: ${command.joinToString(" ")}
                    Exit code: $exitCode
                    Stdout: $output
                    Stderr: $errorOutput
                """.trimIndent())
                return false
            }

            logger.info("known_human checkpoint created successfully")
            if (output.isNotEmpty()) logger.info("git-ai output: $output")
            true
        } catch (e: Exception) {
            logger.warn("Failed to create known_human checkpoint: ${e.message}", e)
            false
        }
    }

    /**
     * Resets the availability check, forcing a re-check on next call.
     * Useful if the user installs git-ai during the session.
     */
    fun resetAvailabilityCheck() {
        synchronized(this) {
            availabilityChecked = false
            cachedVersion = null
            resolvedGitAiPath = null
            lastSearchedPaths = emptyList()
        }
    }

    companion object {
        fun getInstance(): GitAiService = service()
    }
}
