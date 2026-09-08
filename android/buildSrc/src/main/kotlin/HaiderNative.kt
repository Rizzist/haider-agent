package ai.diffforge.haider.build

import org.gradle.api.DefaultTask
import org.gradle.api.file.ConfigurableFileCollection
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.provider.Property
import org.gradle.api.tasks.*
import org.gradle.process.ExecOperations
import javax.inject.Inject

abstract class HaiderNative @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:InputFiles @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val sources: ConfigurableFileCollection
    @get:Internal abstract val repository: DirectoryProperty
    @get:Input abstract val version: Property<String>
    @get:OutputDirectory abstract val outputDirectory: DirectoryProperty
    @TaskAction fun build() {
        exec.exec {
            workingDir(repository.get().asFile)
            commandLine("python3", "scripts/android/build-native.py", "--output", outputDirectory.get().asFile.absolutePath,
                "--version", version.get())
        }
        // Workspace release builds use split-debuginfo=packed. The unstripped
        // ELF alone contains skeleton units; retain its companion beside it.
        val repo = repository.get().asFile
        val target = System.getenv("CARGO_TARGET_DIR")?.let { java.io.File(it) }
            ?.let { if (it.isAbsolute) it else repo.resolve(it.path) } ?: repo.resolve("target")
        val symbols = outputDirectory.get().asFile.resolve("symbols")
        mapOf("arm64-v8a" to "aarch64-linux-android", "x86_64" to "x86_64-linux-android").forEach { (abi, triple) ->
            val elf = symbols.walkTopDown().single { it.isFile && it.name == "libhaider.so" && it.parentFile.name == abi }
            val companion = target.resolve("$triple/release/libhaider.so.dwp")
            check(companion.isFile && companion.length() > 0) { "Missing packed debug companion for $abi" }
            companion.copyTo(elf.parentFile.resolve(companion.name), overwrite = true)
        }
    }
}

abstract class HaiderJniLibs : DefaultTask() {
    @get:InputDirectory @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val nativeDirectory: DirectoryProperty
    @get:Input abstract val abi: Property<String>
    @get:OutputDirectory abstract val outputDirectory: DirectoryProperty
    @TaskAction fun copyLibraries() {
        val output = outputDirectory.get().asFile
        output.deleteRecursively()
        val target = output.resolve("${abi.get()}/libhaider.so")
        target.parentFile.mkdirs()
        nativeDirectory.get().asFile.resolve("jniLibs/${abi.get()}/libhaider.so").copyTo(target)
    }
}
