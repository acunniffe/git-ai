package org.jetbrains.plugins.template.services

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class GitAiBinaryResolverTest {

    @Test
    fun `GIVEN a valid cached path WHEN resolving THEN cached path takes precedence`() {
        val cachedPath = "/cache/git-ai"
        val homeDirectory = "/Users/developer"
        var consolePathReads = 0
        val resolver = resolver(
            homeDirectory = homeDirectory,
            validPaths = setOf(cachedPath, knownLocalDevPath(homeDirectory)),
            consolePath = "/usr/bin/git-ai",
            onConsolePathRead = { consolePathReads++ },
        )

        val resolution = resolver.resolve(cachedPath)

        assertEquals(cachedPath, resolution.executablePath)
        assertEquals(0, consolePathReads)
        assertEquals(
            listOf(knownLocalDevPath(homeDirectory), knownProductionPath(homeDirectory)),
            resolution.searchedPaths,
        )
        assertFalse(resolution.toString().contains("/usr/bin/git-ai"))
    }

    @Test
    fun `GIVEN an invalid cached path WHEN resolving THEN resolution falls through to a known path`() {
        val cachedPath = "/cache/missing-git-ai"
        val homeDirectory = "/Users/developer"
        val localDevPath = knownLocalDevPath(homeDirectory)
        val checkedPaths = mutableListOf<String>()
        val resolver = resolver(
            homeDirectory = homeDirectory,
            validPaths = setOf(localDevPath),
            checkedPaths = checkedPaths,
            consolePath = "/usr/bin/git-ai",
        )

        val resolution = resolver.resolve(cachedPath)

        assertEquals(localDevPath, resolution.executablePath)
        assertEquals(listOf(cachedPath, localDevPath), checkedPaths)
        assertEquals(
            listOf(knownLocalDevPath(homeDirectory), knownProductionPath(homeDirectory)),
            resolution.searchedPaths,
        )
        assertFalse(resolution.toString().contains("/usr/bin/git-ai"))
    }

    @Test
    fun `GIVEN local-dev and production installations WHEN resolving THEN local-dev takes precedence over both`() {
        val homeDirectory = "/Users/developer"
        val localDevPath = knownLocalDevPath(homeDirectory)
        val productionPath = knownProductionPath(homeDirectory)
        val pathExecutable = "/custom/git-ai/bin/git-ai"
        var consolePathReads = 0
        val resolver = resolver(
            homeDirectory = homeDirectory,
            validPaths = setOf(localDevPath, productionPath, pathExecutable),
            consolePath = pathExecutable.substringBeforeLast('/'),
            onConsolePathRead = { consolePathReads++ },
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(localDevPath, resolution.executablePath)
        assertEquals(0, consolePathReads)
        assertEquals(
            listOf(localDevPath, productionPath),
            resolution.searchedPaths,
        )
    }

    @Test
    fun `GIVEN local-dev is absent and production is installed WHEN resolving THEN production path is selected`() {
        val homeDirectory = "/Users/developer"
        val localDevPath = knownLocalDevPath(homeDirectory)
        val productionPath = knownProductionPath(homeDirectory)
        val pathExecutable = "/custom/git-ai/bin/git-ai"
        val checkedPaths = mutableListOf<String>()
        val resolver = resolver(
            homeDirectory = homeDirectory,
            validPaths = setOf(productionPath, pathExecutable),
            checkedPaths = checkedPaths,
            consolePath = pathExecutable.substringBeforeLast('/'),
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(productionPath, resolution.executablePath)
        assertEquals(listOf(localDevPath, productionPath), checkedPaths)
        assertEquals(
            listOf(localDevPath, productionPath),
            resolution.searchedPaths,
        )
    }

    @Test
    fun `GIVEN a quoted PATH entry containing spaces WHEN resolving THEN the entry is normalized before lookup`() {
        val enterpriseDirectory = "/opt/enterprise Git AI/bin"
        val expectedPath = File(enterpriseDirectory, "git-ai").path
        val consolePath = "/usr/local/bin:\"$enterpriseDirectory\":/usr/bin"
        val checkedPaths = mutableListOf<String>()
        val resolver = resolver(
            homeDirectory = null,
            validPaths = setOf(expectedPath),
            consolePath = consolePath,
            pathSeparator = ":",
            checkedPaths = checkedPaths,
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(expectedPath, resolution.executablePath)
        assertEquals(
            listOf("/usr/local/bin/git-ai", expectedPath),
            checkedPaths,
        )
        assertTrue(resolution.searchedPaths.isEmpty())
        assertFalse(resolution.toString().contains(consolePath))
    }

    @Test
    fun `GIVEN a whitespace-only PATH WHEN resolving THEN no executable is returned`() {
        val resolver = resolver(
            homeDirectory = null,
            consolePath = "   ",
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertNull(resolution.executablePath)
        assertTrue(resolution.searchedPaths.isEmpty())
        assertFalse(resolution.toString().contains("   "))
    }

    @Test
    fun `GIVEN a missing PATH WHEN resolving THEN no executable is returned`() {
        val resolver = resolver(
            homeDirectory = null,
            consolePath = null,
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertNull(resolution.executablePath)
        assertTrue(resolution.searchedPaths.isEmpty())
    }

    @Test
    fun `GIVEN Windows and a quoted PATH entry with spaces WHEN resolving THEN git-ai exe is selected`() {
        val windowsDirectory = "C:\\Program Files\\Git AI\\bin"
        val expectedPath = File(windowsDirectory, "git-ai.exe").path
        val consolePath = "\"$windowsDirectory\";C:\\Windows\\System32"
        val checkedPaths = mutableListOf<String>()
        val resolver = resolver(
            homeDirectory = null,
            isWindows = true,
            validPaths = setOf(expectedPath),
            consolePath = consolePath,
            pathSeparator = ";",
            checkedPaths = checkedPaths,
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(expectedPath, resolution.executablePath)
        assertEquals(
            listOf(expectedPath),
            checkedPaths,
        )
        assertTrue(resolution.executablePath.orEmpty().endsWith("git-ai.exe"))
        assertTrue(resolution.searchedPaths.isEmpty())
        assertFalse(resolution.toString().contains(consolePath))
    }

    @Test
    fun `GIVEN a Unix PATH entry with spaces WHEN resolving THEN the complete entry is preserved`() {
        val directoryWithSpaces = "/Applications/Git AI/bin"
        val expectedPath = File(directoryWithSpaces, "git-ai").path
        val consolePath = "/usr/bin:$directoryWithSpaces:/bin"
        val resolver = resolver(
            homeDirectory = null,
            validPaths = setOf(expectedPath),
            consolePath = consolePath,
            pathSeparator = ":",
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(expectedPath, resolution.executablePath)
        assertTrue(resolution.searchedPaths.isEmpty())
        assertFalse(resolution.toString().contains(consolePath))
    }

    @Test
    fun `GIVEN a relative PATH entry WHEN resolving THEN an absolute executable path is returned`() {
        val currentDirectory = "/Applications/Android Studio.app/Contents/bin"
        val expectedPath = File(currentDirectory, "tools/git-ai").toPath().normalize().toString()
        val resolver = resolver(
            homeDirectory = null,
            validPaths = setOf(expectedPath),
            consolePath = "tools:/usr/bin",
            currentDirectory = currentDirectory,
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(expectedPath, resolution.executablePath)
        assertTrue(File(resolution.executablePath.orEmpty()).isAbsolute)
    }

    @Test
    fun `GIVEN an empty PATH entry WHEN resolving THEN the IDE working directory is searched`() {
        val currentDirectory = "/Applications/Android Studio.app/Contents/bin"
        val expectedPath = File(currentDirectory, "git-ai").toPath().normalize().toString()
        val checkedPaths = mutableListOf<String>()
        val resolver = resolver(
            homeDirectory = null,
            validPaths = setOf(expectedPath),
            consolePath = ":/usr/bin",
            currentDirectory = currentDirectory,
            checkedPaths = checkedPaths,
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(expectedPath, resolution.executablePath)
        assertEquals(listOf(expectedPath), checkedPaths)
    }

    @Test
    fun `GIVEN an empty PATH WHEN resolving THEN the IDE working directory is searched`() {
        val currentDirectory = "/Applications/Android Studio.app/Contents/bin"
        val expectedPath = File(currentDirectory, "git-ai").toPath().normalize().toString()
        val resolver = resolver(
            homeDirectory = null,
            validPaths = setOf(expectedPath),
            consolePath = "",
            currentDirectory = currentDirectory,
        )

        val resolution = resolver.resolve(cachedPath = null)

        assertEquals(expectedPath, resolution.executablePath)
        assertTrue(File(resolution.executablePath.orEmpty()).isAbsolute)
    }

    @Test
    fun `GIVEN no executable in known locations or PATH WHEN resolving THEN diagnostics retain searched paths`() {
        val homeDirectory = "/Users/developer"
        val consolePath = "/usr/bin:/opt/custom tools/git-ai/bin"
        val resolver = resolver(
            homeDirectory = homeDirectory,
            validPaths = emptySet(),
            consolePath = consolePath,
        )

        val resolution = resolver.resolve(cachedPath = "/cache/old-git-ai")

        assertNull(resolution.executablePath)
        assertEquals(
            listOf(knownLocalDevPath(homeDirectory), knownProductionPath(homeDirectory)),
            resolution.searchedPaths,
        )
        assertFalse(resolution.toString().contains(consolePath))
    }

    private fun resolver(
        homeDirectory: String?,
        isWindows: Boolean = false,
        validPaths: Set<String> = emptySet(),
        consolePath: String? = null,
        currentDirectory: String = "/ide/working-directory",
        pathSeparator: String = if (isWindows) ";" else ":",
        checkedPaths: MutableList<String> = mutableListOf(),
        onConsolePathRead: () -> Unit = {},
    ): GitAiBinaryResolver = GitAiBinaryResolver(
        homeDirectoryProvider = { homeDirectory },
        isWindowsProvider = { isWindows },
        executableValidator = { path ->
            checkedPaths += path
            path in validPaths
        },
        consolePathProvider = {
            onConsolePathRead()
            consolePath
        },
        currentDirectoryProvider = { File(currentDirectory) },
        pathSeparatorProvider = { pathSeparator },
    )

    private fun knownLocalDevPath(homeDirectory: String): String =
        "$homeDirectory/.git-ai-local-dev/gitwrap/bin/git-ai"

    private fun knownProductionPath(homeDirectory: String): String =
        "$homeDirectory/.git-ai/bin/git-ai"
}
