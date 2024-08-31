use std::path::{Path, PathBuf};

use cargo_subcommand::{Profile, Subcommand};
use ndk_build::error::NdkError;

use ndk_build::ndk::{KeystoreMeta, Ndk};

use crate::Error;
use crate::manifest::Manifest;

pub struct AabBuilder {
    pub cmd: Subcommand,
    pub ndk: Ndk,
    pub crate_path: PathBuf,
    pub manifest: Manifest,
    pub apk_dir: PathBuf,
    pub aab_dir: PathBuf,
    pub java: PathBuf,
    pub jarsigner: PathBuf,
    pub aapt2: PathBuf,
    pub android: PathBuf,
}

impl AabBuilder {
    const APK_TOOL: &'static [u8; 23_137_816] = include_bytes!("../tools/apktool-2.8.1.jar");
    const BUNDLE_TOOL: &'static [u8; 29_069_641] = include_bytes!("../tools/bundletool-1.15.4.jar");

    pub fn from_subcommand(cmd: Subcommand) -> anyhow::Result<Self> {
        let ndk = Ndk::from_env()?;
        let manifest = Manifest::parse_from_toml(cmd.manifest())?;
        let crate_path = PathBuf::from(dunce::simplified(cmd.manifest()).parent().ok_or(NdkError::PathNotFound(PathBuf::from(cmd.manifest())))?);

        let base_dir = dunce::simplified(cmd.target_dir()).join(cmd.profile());
        let apk_dir = base_dir.join("apk");
        let aab_dir = base_dir.join("aab");

        // Get java and jarsigner from JAVA_HOME
        let java = dunce::simplified(std::env::var("JAVA_HOME")?.as_ref()).join("bin").join("java");
        let jarsigner = dunce::simplified(std::env::var("JAVA_HOME")?.as_ref()).join("bin").join("jarsigner");
        let aapt2 = dunce::simplified(std::env::var("ANDROID_HOME")?.as_ref()).join("build-tools").join("35.0.0").join("aapt2");
        let android = dunce::simplified(std::env::var("ANDROID_HOME")?.as_ref()).join("platforms").join("android-35").join("android.jar");

        Ok(Self { cmd, ndk, crate_path, manifest, apk_dir, aab_dir, java, jarsigner, aapt2, android })
    }

    pub fn create_from_apk(&self) -> anyhow::Result<()> {
        let Self { aab_dir, apk_dir, java, jarsigner, aapt2, android, .. } = self;

        std::fs::create_dir_all(&aab_dir)?;
        for entry in std::fs::read_dir(&aab_dir)? {
            let entry = entry?;
            if entry.file_name() != "tools" {
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(entry.path())?;
                } else {
                    std::fs::remove_file(entry.path())?;
                }
            }
        }

        let tools_dir = aab_dir.join("tools");
        std::fs::create_dir_all(&tools_dir)?;

        let apk_tool = tools_dir.join("apktool-2.8.1.jar");
        let bundle_tool = tools_dir.join("bundletool-1.15.4.jar");

        std::fs::write(&apk_tool, Self::APK_TOOL)?;
        std::fs::write(&bundle_tool, Self::BUNDLE_TOOL)?;

        let unpacked_apk = aab_dir.join("unpacked-apk");
        let res_zip = aab_dir.join("res.zip");
        let base_zip = aab_dir.join("base.zip");

        let output = std::process::Command::new(&java)
            .arg("-jar").arg(&apk_tool)
            .arg("d")
            .arg(apk_dir.join(match &self.manifest.apk_name {
                Some(name) => format!("{name}.apk"),
                None => "app.apk".to_string(),
            }))
            .arg("-s")
            .arg("-o").arg(&unpacked_apk)
            .arg("-f")
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to unpack apk: {}", String::from_utf8_lossy(&output.stderr)));
        } else {
            println!("Unpacked apk to {:?}", &unpacked_apk);
        }

        let output = std::process::Command::new(&aapt2)
            .arg("compile")
            .arg("--dir").arg(unpacked_apk.join("res"))
            .arg("-o").arg(&res_zip)
            .output()?;
        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to compile resources: {}", String::from_utf8_lossy(&output.stderr)));
        } else {
            println!("Compiled resources to {:?}", &res_zip);
        }

        let output = std::process::Command::new(&aapt2)
            .arg("link")
            .arg("-o").arg(&base_zip)
            .arg("-R").arg(&res_zip)
            .arg("-I").arg(android)
            .arg("--manifest").arg(unpacked_apk.join("AndroidManifest.xml"))
            .arg("--min-sdk-version").arg(self.manifest.android_manifest.sdk.min_sdk_version.unwrap_or(21).to_string())
            .arg("--target-sdk-version").arg(self.manifest.android_manifest.sdk.target_sdk_version.unwrap_or(35).to_string())
            .arg("--version-code").arg(self.manifest.version_code.unwrap_or(1).to_string())
            .arg("--version-name").arg(self.manifest.version_name.as_deref().unwrap_or("1.0"))
            .arg("--auto-add-overlay")
            .arg("--proto-format")
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to link resources: {}", String::from_utf8_lossy(&output.stderr)));
        } else {
            println!("Linked resources to {:?}", &base_zip);
        }

        let bundle_dir = aab_dir.join("bundle");
        let dex_dir = bundle_dir.join("dex");
        let manifest_dir = bundle_dir.join("manifest");
        let root_dir = bundle_dir.join("root");

        std::fs::create_dir_all(&dex_dir)?;
        std::fs::create_dir(&manifest_dir)?;
        std::fs::create_dir(&root_dir)?;

        let output = std::process::Command::new("unzip")
            .arg("-d").arg(&bundle_dir)
            .arg(&base_zip)
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to unzip base.zip: {}", String::from_utf8_lossy(&output.stderr)));
        } else {
            println!("Unzipped base.zip to {:?}", &bundle_dir);
        }

        std::fs::rename(bundle_dir.join("AndroidManifest.xml"), manifest_dir.join("AndroidManifest.xml"))?;
        std::fs::rename(unpacked_apk.join("lib"), bundle_dir.join("lib"))?;

        if let Err(err) = std::fs::rename(unpacked_apk.join("assets"), bundle_dir.join("assets")) {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(err.into());
            }
        }
        if let Err(err) = std::fs::rename(unpacked_apk.join("unknown"), &root_dir) {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(err.into());
            }
        }
        if let Err(err) = std::fs::rename(unpacked_apk.join("kotlin"), &root_dir) {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(err.into());
            }
        }

        let bundle_zip = bundle_dir.join("bundle.zip");
        let output = std::process::Command::new("jar")
            .arg("cMf").arg(&bundle_zip)
            .arg("-C").arg(&bundle_dir).arg("assets")
            .arg("-C").arg(&bundle_dir).arg("dex")
            .arg("-C").arg(&bundle_dir).arg("lib")
            .arg("-C").arg(&bundle_dir).arg("manifest")
            .arg("-C").arg(&bundle_dir).arg("res")
            .arg("-C").arg(&bundle_dir).arg("root")
            .arg("-C").arg(&bundle_dir).arg("resources.pb")
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to create bundle.zip: {}", String::from_utf8_lossy(&output.stderr)));
        } else {
            println!("Created bundle.zip at {:?}", &bundle_zip);
        }

        let bundle = match &self.manifest.apk_name {
            Some(bundle) => format!("{bundle}-unsigned.aab"),
            None => "bundle-unsigned.aab".to_string(),
        };
        let output = std::process::Command::new(&java)
            .arg("-jar").arg(&bundle_tool)
            .arg("build-bundle")
            .arg("--modules").arg(&bundle_zip)
            .arg("--output").arg(aab_dir.join(&bundle))
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to build bundle: {}", String::from_utf8_lossy(&output.stderr)));
        } else {
            println!("Built bundle at {:?}", aab_dir.join(&bundle));
        }

        let signed = match &self.manifest.apk_name {
            Some(signed) => format!("{signed}.aab"),
            None => "bundle.aab".to_string(),
        };
        let key = self.read_keystore_meta(&self.crate_path, false)?;

        let mut cmd = std::process::Command::new(&jarsigner);
        cmd.arg("-verbose")
           .arg("-sigalg").arg("SHA256withRSA")
           .arg("-digestalg").arg("SHA-256")
           .arg("-keystore").arg(&key.path)
           .arg("-storepass").arg(&key.store_pass)
           .arg("-keypass").arg(&key.key_pass.unwrap_or_default())
           .arg("-signedjar").arg(aab_dir.join(&signed))
           .arg(aab_dir.join(bundle))
           .arg(&key.alias.unwrap_or_default());

        cmd.stdin(std::process::Stdio::null())
           .stdout(std::process::Stdio::inherit())
           .stderr(std::process::Stdio::inherit());
        
        let output = cmd.output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to sign aab: {}", String::from_utf8_lossy(&output.stderr)));
        } else {
            println!("Signed aab at {:?}", aab_dir.join(signed));
        }

        Ok(())
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
}