package ai.diffforge.haider.build

data class HaiderVersion(
    val name: String,
    val code: Int,
) {
    companion object {
        private val VERSION = Regex("""^(\d+)\.(\d+)\.(\d+)$""")
        private val WORKSPACE_PACKAGE = Regex("""(?m)^\s*\[workspace\.package]\s*$""")
        private val TABLE = Regex("""(?m)^\s*\[[^]]+]\s*$""")
        private val VERSION_FIELD = Regex("""(?m)^\s*version\s*=\s*"([^"]+)"\s*(?:#.*)?$""")

        fun parse(value: String): HaiderVersion {
            val match = VERSION.matchEntire(value.trim())
                ?: error("Haider version must be major.minor.patch, got '$value'")
            val (major, minor, patch) = match.destructured.toList().map { it.toInt() }
            require(major <= 2_100) { "Haider major version is too large for Android versionCode" }
            require(minor <= 99) { "Haider minor version must be <= 99 for Android versionCode" }
            require(patch <= 9_999) { "Haider patch version must be <= 9999 for Android versionCode" }
            val code = Math.addExact(
                Math.addExact(Math.multiplyExact(major, 1_000_000), Math.multiplyExact(minor, 10_000)),
                patch,
            )
            require(code in 1..2_100_000_000) { "Android versionCode must be between 1 and 2100000000" }
            return HaiderVersion(match.value, code)
        }

        fun fromWorkspaceManifest(contents: String): HaiderVersion {
            val workspace = WORKSPACE_PACKAGE.find(contents)
                ?: error("Cargo.toml is missing [workspace.package]")
            val tableStart = workspace.range.last + 1
            val nextTable = TABLE.find(contents, tableStart)?.range?.first ?: contents.length
            val table = contents.substring(tableStart, nextTable)
            val value = VERSION_FIELD.find(table)?.groupValues?.get(1)
                ?: error("Cargo.toml [workspace.package] is missing version")
            return parse(value)
        }
    }
}
