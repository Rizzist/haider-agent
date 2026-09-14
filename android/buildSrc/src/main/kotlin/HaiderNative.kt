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
    @get:Input abstract val verifyOnly: Property<Boolean>
    @get:OutputDirectory abstract val outputDirectory: DirectoryProperty
    @TaskAction fun build() {
        exec.exec {
            workingDir(repository.get().asFile)
            val command = mutableListOf("python3", "scripts/android/build-native.py", "--output",
                outputDirectory.get().asFile.absolutePath, "--version", version.get())
            if (verifyOnly.get()) command.add("--verify-only")
            commandLine(command)
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
