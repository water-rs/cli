plugins {
    id("com.android.library")
    `maven-publish`
}

android {
    namespace = "{{ ctx.bundle_identifier }}.waterui"
    compileSdk = 37

    defaultConfig {
        minSdk = {{ ctx.android_min_api_level() }}
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_{{ ctx.android_jdk_version() }}
        targetCompatibility = JavaVersion.VERSION_{{ ctx.android_jdk_version() }}
    }

    // Staged dependency Kotlin sources and vendored jars are only reached by
    // name (JNI loadClass); consumer apps minifying with R8 must keep them.
    // The CLI's android classpath staging maintains the keep block in this
    // file; consumerProguardFiles ships it inside the AAR.
    consumerProguardFiles("proguard-rules.pro")

    publishing {
        singleVariant("release")
    }
}

group = "{{ ctx.bundle_identifier }}"
version = "{{ ctx.crate_version }}"

dependencies {
    // Vendored jars dependency crates stage here at package time (the CLI's
    // android classpath staging writes into `libs/`).
    api(fileTree("libs") { include("*.jar") })

    // Exported (`api`) so the host's compile classpath sees WaterUiRootView.
    api("{{ ctx.android_runtime_dependency() }}")
}

publishing {
    publications {
        create<MavenPublication>("release") {
            groupId = "{{ ctx.bundle_identifier }}"
            artifactId = "{{ ctx.crate_name }}"
            version = "{{ ctx.crate_version }}"

            // `singleVariant("release")` registers the component late, so the
            // publication wires it after evaluation, like the runtime itself.
            afterEvaluate {
                from(components["release"])
            }
        }
    }
}
