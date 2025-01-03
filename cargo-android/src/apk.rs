use std::path::{Path, PathBuf};

use cargo_subcommand::{Artifact, ArtifactType, CrateType, Profile, Subcommand};

use ndk_build::apk::{Apk, ApkConfig};
use ndk_build::cargo::{cargo_ndk, VersionCode};
use ndk_build::dylibs::get_libs_search_paths;
use ndk_build::error::NdkError;
use ndk_build::manifest::{IntentFilter, MetaData};
use ndk_build::ndk::{KeystoreMeta, Ndk};
use ndk_build::target::Target;

use crate::error::Error;
use crate::manifest::{Inheritable, Manifest, Root};

pub struct ApkBuilder<'a> {
    cmd: &'a Subcommand,
    ndk: Ndk,
    manifest: Manifest,
    build_dir: PathBuf,
    build_targets: Vec<Target>,
    device_serial: Option<String>,
}

impl<'a> ApkBuilder<'a> {
    pub fn from_subcommand(cmd: &'a Subcommand, device_serial: Option<String>) -> Result<Self, Error> {
        println!(
            "Using package `{}` in `{}`",
            cmd.package(),
            cmd.manifest().display()
        );
        let ndk = Ndk::from_env()?;
        let mut manifest = Manifest::parse_from_toml(cmd.manifest())?;
        let workspace_manifest: Option<Root> = cmd
            .workspace_manifest()
            .map(Root::parse_from_toml)
            .transpose()?;
        let build_targets = if let Some(target) = cmd.target() {
            vec![Target::from_rust_triple(target)?]
        } else if !manifest.build_targets.is_empty() {
            manifest.build_targets.clone()
        } else {
            vec![ndk
                .detect_abi(device_serial.as_deref())
                .unwrap_or(Target::Arm64V8a)]
        };
        let build_dir = dunce::simplified(cmd.target_dir())
            .join(cmd.profile())
            .join("apk");

        let package_version = match &manifest.version {
            Inheritable::Value(v) => v.clone(),
            Inheritable::Inherited { workspace: true } => {
                let workspace = workspace_manifest
                    .ok_or(Error::InheritanceMissingWorkspace)?
                    .workspace
                    .unwrap_or_else(|| {
                        // Unlikely to fail as cargo-subcommand should give us
                        // a `Cargo.toml` containing a `[workspace]` table
                        panic!(
                            "Manifest `{:?}` must contain a `[workspace]` table",
                            cmd.workspace_manifest().unwrap()
                        )
                    });

                workspace
                    .package
                    .ok_or(Error::WorkspaceMissingInheritedField("package"))?
                    .version
                    .ok_or(Error::WorkspaceMissingInheritedField("package.version"))?
            }
            Inheritable::Inherited { workspace: false } => return Err(Error::InheritedFalse),
        };
        let version_code = VersionCode::from_semver(&package_version)?.to_code(1);

        // Set default Android manifest values
        if manifest
            .android_manifest
            .version_name
            .replace(package_version)
            .is_some()
        {
            panic!("version_name should not be set in TOML");
        }

        if manifest
            .android_manifest
            .version_code
            .replace(version_code)
            .is_some()
        {
            panic!("version_code should not be set in TOML");
        }

        let target_sdk_version = *manifest
            .android_manifest
            .sdk
            .target_sdk_version
            .get_or_insert_with(|| ndk.default_target_platform());

        manifest
            .android_manifest
            .application
            .debuggable
            .get_or_insert_with(|| *cmd.profile() == Profile::Dev);

        let activity = &mut manifest.android_manifest.application.activity;

        // Add a default `MAIN` action to launch the activity, if the user didn't supply it by hand.
        if activity
            .intent_filter
            .iter()
            .all(|i| i.actions.iter().all(|f| f != "android.intent.action.MAIN"))
        {
            activity.intent_filter.push(IntentFilter {
                actions: vec!["android.intent.action.MAIN".to_string()],
                categories: vec!["android.intent.category.LAUNCHER".to_string()],
                data: vec![],
            });
        }

        // Export the sole Rust activity on Android S and up, if the user didn't explicitly do so.
        // Without this, apps won't start on S+.
        // https://developer.android.com/about/versions/12/behavior-changes-12#exported
        if target_sdk_version >= 31 {
            activity.exported.get_or_insert(true);
        }

        Ok(Self {
            cmd,
            ndk,
            manifest,
            build_dir,
            build_targets,
            device_serial,
        })
    }

    pub fn check(&self) -> Result<(), Error> {
        for target in &self.build_targets {
            let mut cargo = cargo_ndk(
                &self.ndk,
                *target,
                self.min_sdk_version(),
                self.cmd.target_dir(),
            )?;
            cargo.arg("check");
            if self.cmd.target().is_none() {
                let triple = target.rust_triple();
                cargo.arg("--target").arg(triple);
            }
            self.cmd.args().apply(&mut cargo);
            if !cargo.status()?.success() {
                return Err(NdkError::CmdFailed(cargo).into());
            }
        }
        Ok(())
    }

    pub fn build(&self, artifact: &Artifact) -> Result<Apk, Error> {
        // Set artifact specific manifest default values.
        let mut manifest = self.manifest.android_manifest.clone();

        if manifest.package.is_empty() {
            let name = artifact.name.replace('-', "_");
            manifest.package = match artifact.r#type {
                ArtifactType::Lib | ArtifactType::Bin => format!("rust.{name}"),
                ArtifactType::Example => format!("rust.example.{name}"),
            };
        }

        if manifest.application.label.is_empty() {
            manifest.application.label = artifact.name.to_string();
        }

        manifest.application.activity.meta_data.push(MetaData {
            name: "android.app.lib_name".to_string(),
            value: artifact.name.replace('-', "_"),
        });

        let crate_path = self.cmd.manifest().parent().expect("invalid manifest path");

        let is_debug_profile = *self.cmd.profile() == Profile::Dev;

        let assets = self
            .manifest
            .assets
            .as_ref()
            .map(|assets| dunce::simplified(&crate_path.join(assets)).to_owned());
        let resources = self
            .manifest
            .resources
            .as_ref()
            .map(|res| dunce::simplified(&crate_path.join(res)).to_owned());
        let runtime_libs = self
            .manifest
            .runtime_libs
            .as_ref()
            .map(|libs| dunce::simplified(&crate_path.join(libs)).to_owned());
        let apk_name = self
            .manifest
            .apk_name
            .clone()
            .unwrap_or_else(|| artifact.name.to_string());

        let config = ApkConfig {
            ndk: self.ndk.clone(),
            build_dir: self.build_dir.join(artifact.build_dir()),
            apk_name,
            assets,
            resources,
            manifest,
            disable_aapt_compression: is_debug_profile,
            strip: self.manifest.strip,
            reverse_port_forward: self.manifest.reverse_port_forward.clone(),
        };
        let mut apk = config.create_apk()?;

        for target in &self.build_targets {
            let triple = target.rust_triple();
            let build_dir = self.cmd.build_dir(Some(triple));
            let artifact = self.cmd.artifact(artifact, Some(triple), CrateType::Cdylib);

            let mut cargo = cargo_ndk(
                &self.ndk,
                *target,
                self.min_sdk_version(),
                self.cmd.target_dir(),
            )?;
            cargo.arg("build");
            if self.cmd.target().is_none() {
                cargo.arg("--target").arg(triple);
            }
            self.cmd.args().apply(&mut cargo);

            if !cargo.status()?.success() {
                return Err(NdkError::CmdFailed(cargo).into());
            }

            let mut libs_search_paths =
                get_libs_search_paths(self.cmd.target_dir(), triple, self.cmd.profile().as_ref())?;
            libs_search_paths.push(build_dir.join("deps"));

            let libs_search_paths = libs_search_paths
                .iter()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>();

            apk.add_lib_recursively(&artifact, *target, libs_search_paths.as_slice())?;

            if let Some(runtime_libs) = &runtime_libs {
                apk.add_runtime_libs(runtime_libs, *target, libs_search_paths.as_slice())?;
            }
        }

        let signing_key = self.read_keystore_meta(crate_path, is_debug_profile)?;

        let unsigned = apk.add_pending_libs_and_align()?;

        println!(
            "Signing `{}` with keystore `{}`",
            config.apk().display(),
            signing_key.path.display()
        );
        Ok(unsigned.sign(signing_key)?)
    }

    fn read_keystore_meta(&self, crate_path: &Path, is_debug_profile: bool) -> Result<KeystoreMeta, Error> {
        let profile_name = match self.cmd.profile() {
            Profile::Dev => "dev",
            Profile::Release => "release",
            Profile::Custom(c) => c.as_str(),
        };

        let manifest = self.manifest.signing.get(profile_name);

        let profile_name = profile_name.to_uppercase().replace('-', "_");

        // TODO: Add documentation for environment variables and signing section

        let env_store_path = format!("CARGO_ANDROID_{profile_name}_STORE_PATH");
        let env_store_password = format!("CARGO_ANDROID_{profile_name}_STORE_PASSWORD");
        let env_key_alias = format!("CARGO_ANDROID_{profile_name}_KEY_ALIAS");
        let env_key_password = format!("CARGO_ANDROID_{profile_name}_KEY_PASSWORD");

        let store_path = std::env::var_os(&env_store_path).map(PathBuf::from);
        let store_password = std::env::var(&env_store_password).ok();
        let key_alias = std::env::var(&env_key_alias).ok();
        let key_password = std::env::var(&env_key_password).ok();

        if let Some(store_path) = store_path {
            let signing_key = match store_password {
                Some(store_password) => KeystoreMeta::single(store_path, store_password),
                None => if is_debug_profile {
                    println!("{env_store_password} not specified, falling back to default password");
                    KeystoreMeta::single(store_path, ndk_build::ndk::DEFAULT_DEV_KEYSTORE_PASSWORD.to_owned())
                } else {
                    eprintln!("`{}` was specified via `{env_store_path}`, but `{env_store_password}` was not specified, both or neither must be present for profiles other than `dev`", store_path.to_string_lossy());
                    return Err(Error::MissingReleaseKey(profile_name));
                },
            };

            return match key_alias {
                Some(key_alias) => if let Some(key_password) = key_password {
                    Ok(signing_key.alias(key_alias).key_pass(key_password))
                } else {
                    eprintln!("`{key_alias}` was specified via `{env_key_alias}`, but `{env_key_password}` was not specified");
                    Err(Error::MissingReleaseKey(profile_name))
                },
                None => Ok(signing_key),
            };
        }

        if let Some(signing) = manifest {
            let store_path = crate_path.join(&signing.store_path);
            let store_password = signing.store_password.clone();
            let key_alias = signing.key_alias.clone();
            let key_password = signing.key_password.clone();

            let signing_key = KeystoreMeta::single(store_path, store_password);

            return match key_alias {
                Some(key_alias) => if let Some(key_password) = key_password {
                    Ok(signing_key.alias(key_alias).key_pass(key_password))
                } else {
                    eprintln!("`{key_alias}` was specified via `{env_key_alias}`, but `{env_key_password}` was not specified");
                    Err(Error::MissingReleaseKey(profile_name))
                },
                None => Ok(signing_key),
            };
        }

        if is_debug_profile {
            Ok(self.ndk.debug_key()?)
        } else {
            Err(Error::MissingReleaseKey(profile_name))
        }
    }

    pub fn run(&self, artifact: &Artifact, no_logcat: bool) -> Result<(), Error> {
        let apk = self.build(artifact)?;
        apk.reverse_port_forwarding(self.device_serial.as_deref())?;
        apk.install(self.device_serial.as_deref())?;
        apk.start(self.device_serial.as_deref())?;
        let uid = apk.uidof(self.device_serial.as_deref())?;

        if !no_logcat {
            self.ndk
                .adb(self.device_serial.as_deref())?
                .arg("logcat")
                .arg("-v")
                .arg("color")
                .arg("--uid")
                .arg(uid.to_string())
                .status()?;
        }

        Ok(())
    }

    pub fn gdb(&self, artifact: &Artifact) -> Result<(), Error> {
        let apk = self.build(artifact)?;
        apk.install(self.device_serial.as_deref())?;

        let target_dir = self.build_dir.join(artifact.build_dir());
        self.ndk.ndk_gdb(
            target_dir,
            "android.app.NativeActivity",
            self.device_serial.as_deref(),
        )?;
        Ok(())
    }

    pub fn default(&self, cargo_cmd: &str, cargo_args: &[String]) -> Result<(), Error> {
        for target in &self.build_targets {
            let mut cargo = cargo_ndk(
                &self.ndk,
                *target,
                self.min_sdk_version(),
                self.cmd.target_dir(),
            )?;
            cargo.arg(cargo_cmd);
            self.cmd.args().apply(&mut cargo);

            if self.cmd.target().is_none() {
                let triple = target.rust_triple();
                cargo.arg("--target").arg(triple);
            }

            for additional_arg in cargo_args {
                cargo.arg(additional_arg);
            }

            if !cargo.status()?.success() {
                return Err(NdkError::CmdFailed(cargo).into());
            }
        }
        Ok(())
    }

    /// Returns `minSdkVersion` for use in compiler target selection:
    /// <https://developer.android.com/ndk/guides/sdk-versions#minsdkversion>
    ///
    /// Has a lower bound of `23` to retain backwards compatibility with
    /// the previous default.
    fn min_sdk_version(&self) -> u32 {
        self.manifest
            .android_manifest
            .sdk
            .min_sdk_version
            .unwrap_or(23)
            .max(23)
    }
}