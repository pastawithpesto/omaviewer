import java.util.Properties

plugins { id("com.android.application") }

val androidAbis = providers.gradleProperty("androidAbis")
    .orElse(providers.environmentVariable("ANDROID_ABIS"))
    .orElse("arm64-v8a").get().split(",").map { it.trim() }.distinct()
val rustTriples = mapOf("arm64-v8a" to "aarch64-linux-android", "x86_64" to "x86_64-linux-android")
require(androidAbis.isNotEmpty() && androidAbis.all { it in rustTriples }) {
    "androidAbis / ANDROID_ABIS must contain arm64-v8a and/or x86_64 (comma separated)"
}
val rustJniLibs = layout.buildDirectory.dir("generated/rustJniLibs")
val explicitNdkPath = providers.environmentVariable("ANDROID_NDK_HOME")
    .orElse(providers.environmentVariable("ANDROID_NDK_ROOT")).orNull
val explicitNdkVersion = explicitNdkPath?.let { path ->
    val properties = file("$path/source.properties")
    if (properties.isFile) Properties().apply {
        properties.inputStream().use { load(it) }
    }.getProperty("Pkg.Revision") else null
}

android {
    namespace = "dev.windowlink.client"
    compileSdk = 35
    ndkVersion = providers.gradleProperty("androidNdkVersion")
        .orElse(providers.environmentVariable("ANDROID_NDK_VERSION"))
        .orElse(explicitNdkVersion ?: "27.2.12479018").get()
    explicitNdkPath?.let { ndkPath = it }
    defaultConfig {
        applicationId = "dev.windowlink.client"
        minSdk = 26
        // API 35 enforces edge-to-edge; NativeActivity's egui window does not apply
        // Android view insets yet, so target 34 to keep controls below system bars.
        targetSdk = 34
        versionCode = 3
        versionName = "0.1.0"
        ndk { abiFilters += androidAbis }
    }
    // Replace the old source directory: manually copied/stale ABI libraries must
    // never be packaged alongside the selected generated libraries.
    sourceSets["main"].jniLibs.setSrcDirs(listOf(rustJniLibs))
}

val selectedNdk = androidComponents.sdkComponents.ndkDirectory
val buildRustAndroid by tasks.registering(Exec::class) {
    workingDir = rootProject.projectDir
    commandLine(listOf("cargo", "ndk") + androidAbis.flatMap { listOf("-t", it) } +
        listOf("--platform", "26", "-o", rustJniLibs.get().asFile.absolutePath, "build", "--release", "--locked", "--lib"))
    inputs.property("rustCompiler", providers.exec {
        workingDir(rootProject.projectDir)
        commandLine("rustc", "--version", "--verbose")
    }.standardOutput.asText)
    inputs.property("abis", androidAbis)
    inputs.property("ndkVersion", android.ndkVersion)
    inputs.property("ndkPath", selectedNdk.map { it.asFile.absolutePath })
    for (name in listOf("RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_HOME", "CARGO_TARGET_DIR", "PATH")) {
        inputs.property(name, providers.environmentVariable(name).orElse(""))
    }
    inputs.files(
        rootProject.fileTree("src"),
        rootProject.fileTree("assets"),
        rootProject.fileTree(".cargo"),
        rootProject.file("Cargo.toml"),
        rootProject.file("Cargo.lock"),
        rootProject.fileTree(".") { include("build.rs", "rust-toolchain", "rust-toolchain.toml") },
        rootProject.fileTree("../shared") { exclude("target/**", ".git/**") },
        rootProject.fileTree("../vendor/ironrdp") { exclude("**/target/**", "**/.git/**") },
        selectedNdk.map { it.file("source.properties") },
    )
    outputs.dir(rustJniLibs)
    for (abi in androidAbis) {
        outputs.file(rustJniLibs.map { it.file("$abi/libwindowlink_android.so") })
        outputs.file(rustJniLibs.map { it.file("$abi/libc++_shared.so") })
    }
    doFirst {
        environment("ANDROID_NDK_HOME", selectedNdk.get().asFile.absolutePath)
        delete(rustJniLibs)
    }
    doLast {
        val prebuilts = selectedNdk.get().dir("toolchains/llvm/prebuilt").asFile
        for (abi in androidAbis) {
            val runtimes = fileTree(prebuilts) {
                include("*/sysroot/usr/lib/${rustTriples.getValue(abi)}/libc++_shared.so")
            }.files
            check(runtimes.size == 1) { "Expected one NDK libc++_shared.so for $abi, found ${runtimes.size} in $prebuilts" }
            copy { from(runtimes.single()); into(rustJniLibs.get().dir(abi)) }
        }
    }
}

tasks.named("preBuild").configure { dependsOn(buildRustAndroid) }
