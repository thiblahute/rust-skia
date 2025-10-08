use std::{fs, io};

use build_support::{
    binaries_config,
    cargo::{self, Target},
    features, platform, skia, skia_bindgen,
};

mod build_support;

fn main() -> Result<(), io::Error> {
    if env::is_docs_rs_build() {
        println!("DETECTED DOCS_RS BUILD");
        return fake_bindings();
    }

    // Check if we should use pkg-config to find skia
    if env::use_pkg_config() {
        println!("USING PKG-CONFIG TO FIND SKIA");

        let features = features::Features::from_cargo_env();

        let libs = if features[features::feature::TEXTLAYOUT] {
            &[
                "skia",
                "skparagraph",
                "skshaper",
                "skunicode_core",
                "skunicode_icu",
            ][..]
        } else {
            &["skia"][..]
        };

        // Probe all libraries and collect defines from all of them
        let mut all_defines = std::collections::HashMap::new();
        for lib in libs {
            let lib_info = pkg_config::Config::new()
                .probe(lib)
                .map_err(|e| io::Error::other(format!("pkg-config failed for {}: {}", lib, e)))?;

            // Merge defines from this library
            for (name, value) in lib_info.defines.iter() {
                all_defines.insert(name.clone(), value.clone());
            }
        }

        // pkg-config already emits the necessary cargo directives via its probe() call,
        // but we still need to generate bindings

        // Get the skia include directory from pkg-config variable
        let skia_source_dir = env::source_dir().unwrap_or_else(|| {
            pkg_config::get_variable("skia", "skia_includedir")
                .ok()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap().join("skia"))
        });

        let cargo_target = cargo::target();

        // Convert defines to Vec for generate_bindings
        // These defines come from ALL libraries and will be used for both bindgen and cc
        let mut definitions: Vec<(String, Option<String>)> = all_defines.into_iter().collect();

        let binaries_config =
            binaries_config::BinariesConfiguration::from_features(&features, env::is_skia_debug());

        // Generate bindings - the definitions will be passed to both bindgen and cc
        generate_bindings(
            &features,
            definitions,
            &binaries_config,
            &skia_source_dir,
            cargo_target.clone(),
            None,
        );

        // Link the compiled bindings library (but not the Skia libraries, those come from pkg-config)
        cargo::add_link_search(binaries_config.output_directory.to_str().unwrap());
        cargo::add_static_link_libs(
            &cargo_target,
            binaries_config.binding_libraries.iter().map(|s| s.as_str()),
        );
        cargo::add_link_libs(&binaries_config.link_libraries);

        return Ok(());
    } else {
        println!("NOT USING PKG-CONFIG TO FIND SKIA");
    }

    let skia_debug = env::is_skia_debug();
    let cargo_target = cargo::target();

    let features = {
        let mut features = features::Features::from_cargo_env();
        let missing_dependencies = features.missing_dependencies();
        if !missing_dependencies.is_empty() {
            return Err(io::Error::other(format!(
                "Missing dependent features: {missing_dependencies}"
            )));
        }

        let redundant_features = platform::redundant_features(&features, &cargo_target);
        if !redundant_features.is_empty() {
            #[cfg(feature = "binary-cache")]
            if build_support::binary_cache::should_export().is_some() {
                return Err(io::Error::other(format!(
                    "Can't produce binaries with redundant features: {redundant_features}"
                )));
            }
            cargo::warning(format!(
                "Redundant features: {redundant_features}. Disabled for the build and binary download."
            ));
            features -= redundant_features;
            cargo::warning(format!("Final features: {features}"));
        }

        features
    };

    let binaries_config =
        binaries_config::BinariesConfiguration::from_features(&features, skia_debug);

    //
    // skip attempting to download?
    //
    if let Some(source_dir) = env::source_dir() {
        if let Some(search_path) = env::skia_lib_search_path() {
            println!("STARTING BIND AGAINST SYSTEM SKIA");

            binaries_config.import(&search_path, false).unwrap();

            let definitions = skia_bindgen::definitions::from_env();
            generate_bindings(
                &features,
                definitions,
                &binaries_config,
                &source_dir,
                cargo_target,
                None,
            );
        } else {
            if cfg!(feature = "no-compile") {
                panic!("Refusing to offline-build skia with no-compile feature");
            }

            println!("STARTING OFFLINE BUILD");

            let final_build_configuration = build_from_source(
                features.clone(),
                &binaries_config,
                &source_dir,
                skia_debug,
                true,
            );
            let definitions = skia_bindgen::definitions::from_ninja_features(
                &features,
                final_build_configuration.use_system_libraries,
                &binaries_config.output_directory,
            );
            generate_bindings(
                &features,
                definitions,
                &binaries_config,
                &source_dir,
                final_build_configuration.target,
                final_build_configuration
                    .sysroot
                    .as_ref()
                    .map(AsRef::as_ref),
            );
        }
    } else {
        //
        // is the download of prebuilt binaries possible?
        //

        #[allow(unused_variables)]
        let build_skia = true;

        #[cfg(feature = "binary-cache")]
        let build_skia = build_support::binary_cache::try_prepare_download(&binaries_config);

        //
        // full build?
        //

        if build_skia {
            if cfg!(feature = "no-compile") {
                panic!("Refusing to full-build skia with no-compile feature");
            }

            println!("STARTING A FULL BUILD");
            println!("HOST: {}", cargo::host());

            let source_dir = std::env::current_dir().unwrap().join("skia");
            let final_build_configuration = build_from_source(
                features.clone(),
                &binaries_config,
                &source_dir,
                skia_debug,
                false,
            );
            let definitions = skia_bindgen::definitions::from_ninja_features(
                &features,
                final_build_configuration.use_system_libraries,
                &binaries_config.output_directory,
            );
            generate_bindings(
                &features,
                definitions,
                &binaries_config,
                &source_dir,
                final_build_configuration.target,
                final_build_configuration
                    .sysroot
                    .as_ref()
                    .map(AsRef::as_ref),
            );
        }
    };

    binaries_config.commit_to_cargo();

    #[cfg(feature = "binary-cache")]
    if let Some(staging_directory) = build_support::binary_cache::should_export() {
        build_support::binary_cache::publish(&binaries_config, &staging_directory);
    }

    Ok(())
}

fn build_from_source(
    features: features::Features,
    binaries_config: &binaries_config::BinariesConfiguration,
    skia_source_dir: &std::path::Path,
    skia_debug: bool,
    offline: bool,
) -> skia::FinalBuildConfiguration {
    let build_config = skia::BuildConfiguration::from_features(features, skia_debug);
    let final_configuration = skia::FinalBuildConfiguration::from_build_configuration(
        &build_config,
        skia::env::use_system_libraries(),
        skia_source_dir,
    );

    skia::build(
        &final_configuration,
        binaries_config,
        skia::env::ninja_command(),
        skia::env::gn_command(),
        offline,
    );

    final_configuration
}

fn generate_bindings(
    features: &features::Features,
    definitions: Vec<skia_bindgen::Definition>,
    binaries_config: &binaries_config::BinariesConfiguration,
    skia_source_dir: &std::path::Path,
    target: Target,
    sysroot: Option<&str>,
) {
    // Emit the ninja definitions, to help debug build consistency.
    skia_bindgen::definitions::save_definitions(&definitions, &binaries_config.output_directory)
        .expect("failed to write Skia defines");

    let bindings_config = skia_bindgen::Configuration::new(features, definitions, skia_source_dir);
    skia_bindgen::generate_bindings(
        &bindings_config,
        &binaries_config.output_directory,
        target,
        sysroot,
    );
}

/// On docs.rs, rustdoc runs inside a container with no networking, so copy a pre-generated
/// `bindings.rs` file.
fn fake_bindings() -> Result<(), io::Error> {
    println!("COPYING bindings_docs.rs to OUT_DIR/skia/bindings.rs");
    let bindings_target = cargo::output_directory()
        .join(binaries_config::SKIA_OUTPUT_DIR)
        .join("bindings.rs");
    fs::copy("bindings_docs.rs", bindings_target).map(|_| ())
}

/// Environment variables used by this build script.
mod env {
    use crate::build_support::cargo;
    use std::path::PathBuf;

    /// The path to the Skia source directory.
    pub fn source_dir() -> Option<PathBuf> {
        cargo::env_var("SKIA_SOURCE_DIR").map(PathBuf::from)
    }

    /// The path to where a pre-built Skia library can be found.
    pub fn skia_lib_search_path() -> Option<PathBuf> {
        cargo::env_var("SKIA_LIBRARY_SEARCH_PATH").map(PathBuf::from)
    }

    pub fn is_skia_debug() -> bool {
        matches!(cargo::env_var("SKIA_DEBUG"), Some(v) if v != "0")
    }

    pub fn is_docs_rs_build() -> bool {
        matches!(cargo::env_var("DOCS_RS"), Some(v) if v != "0")
    }

    /// Whether to use pkg-config to find skia.
    pub fn use_pkg_config() -> bool {
        matches!(cargo::env_var("SKIA_USE_PKG_CONFIG"), Some(v) if v == "true" || v == "1")
    }
}
