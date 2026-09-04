package org.jetbrains.plugins.template.services

import com.intellij.util.EnvironmentUtil
import java.io.File

/**
 * Resolves the git-ai executable without invoking a shell.
 *
 * The environment supplied by [EnvironmentUtil] is the environment IntelliJ loaded for
 * processes launched from the IDE. On macOS this can include the user's shell PATH even when
 * the IDE itself was started from a graphical launcher.
 */
internal class GitAiBinaryResolver(
    private val homeDirectoryProvider: () -> String? = { System.getProperty("user.home") },
    private val isWindowsProvider: () -> Boolean = {
        System.getProperty("os.name").orEmpty().contains("win", ignoreCase = true)
    },
    private val executableValidator: (String) -> Boolean = { path ->
        File(path).isFile && File(path).canExecute()
    },
    private val consolePathProvider: () -> String? = {
        EnvironmentUtil.getValue("PATH")
    },
    private val currentDirectoryProvider: () -> File = {
        File(System.getProperty("user.dir"))
    },
    private val pathSeparatorProvider: () -> String = {
        if (isWindowsProvider()) ";" else File.pathSeparator
    },
) {

    /** The result includes the known locations searched during resolution. */
    internal data class Resolution(
        val executablePath: String?,
        val searchedPaths: List<String>,
    )

    /**
     * Resolves in precedence order: cached path, standard installation locations, then the
     * PATH loaded by IntelliJ.
     */
    internal fun resolve(cachedPath: String?): Resolution {
        val knownPaths = knownPaths()

        if (cachedPath != null && executableValidator(cachedPath)) {
            return Resolution(
                executablePath = cachedPath,
                searchedPaths = knownPaths,
            )
        }

        knownPaths.firstOrNull(executableValidator)?.let { path ->
            return Resolution(
                executablePath = path,
                searchedPaths = knownPaths,
            )
        }

        val pathExecutable = findOnPath(consolePathProvider())
        return Resolution(
            executablePath = pathExecutable,
            searchedPaths = knownPaths,
        )
    }

    private fun knownPaths(): List<String> {
        val homeDirectory = homeDirectoryProvider()?.takeIf { it.isNotBlank() } ?: return emptyList()

        return if (isWindowsProvider()) {
            listOf(
                "$homeDirectory\\.git-ai-local-dev\\gitwrap\\bin\\git-ai.exe",
                "$homeDirectory\\.git-ai\\bin\\git-ai.exe",
            )
        } else {
            listOf(
                "$homeDirectory/.git-ai-local-dev/gitwrap/bin/git-ai",
                "$homeDirectory/.git-ai/bin/git-ai",
            )
        }
    }

    private fun findOnPath(pathValue: String?): String? {
        if (pathValue == null || (pathValue.isNotEmpty() && pathValue.isBlank())) return null

        val executableNames = if (isWindowsProvider()) {
            listOf("git-ai.exe", "git-ai")
        } else {
            listOf("git-ai")
        }

        return pathValue
            .split(pathSeparatorProvider())
            .asSequence()
            .map { it.normalizePathEntry() }
            .flatMap { directory ->
                executableNames.asSequence().map { executableName ->
                    resolvePathEntry(directory, executableName)
                }
            }
            .firstOrNull(executableValidator)
    }

    private fun resolvePathEntry(directory: String, executableName: String): String {
        val workingDirectory = currentDirectoryProvider().absoluteFile.toPath().normalize().toFile()
        val pathDirectory = when {
            directory.isEmpty() -> workingDirectory
            File(directory).isAbsolute -> File(directory)
            isWindowsProvider() && directory.isAbsoluteWindowsPath() -> File(directory)
            else -> File(workingDirectory, directory)
        }
        val executable = File(pathDirectory, executableName)

        return if (executable.isAbsolute) {
            executable.toPath().normalize().toString()
        } else {
            // Windows paths are not considered absolute when tests run on another host OS.
            executable.path
        }
    }

    private fun String.normalizePathEntry(): String = trim().removeSurrounding("\"")

    private fun String.isAbsoluteWindowsPath(): Boolean =
        matches(Regex("^[A-Za-z]:[\\\\/].*")) || startsWith("\\\\\\\\")
}
