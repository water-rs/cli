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

    publishing {
        singleVariant("release")
    }
}

group = "{{ ctx.bundle_identifier }}"
version = "{{ ctx.crate_version }}"

dependencies {
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
