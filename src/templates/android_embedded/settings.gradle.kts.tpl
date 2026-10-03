import org.gradle.api.initialization.resolve.RepositoriesMode

pluginManagement {
    repositories {
        google()
        maven { url = uri("https://dl.google.com/dl/android/maven2/") }
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        maven { url = uri("https://dl.google.com/dl/android/maven2/") }
        mavenCentral()
        // The WaterUI Android runtime resolves through JitPack unless a local
        // backend checkout substitutes it below.
        if ({{ ctx.use_remote_dev_backend() }}) {
            maven {
                url = uri("https://jitpack.io")
            }
        }
    }
}

rootProject.name = "{{ ctx.app_name }}-embedded"
include(":waterui")

// For local dev mode: uses the android-backend checkout the project resolves
// through a local WaterUI tree's `backends/android` checkout.
if (!{{ ctx.use_remote_dev_backend() }}) {
    includeBuild("{{ ctx.android_backend_path() }}") {
        dependencySubstitution {
            substitute(module("dev.waterui.android:runtime")).using(project(":runtime"))
        }
    }
}
