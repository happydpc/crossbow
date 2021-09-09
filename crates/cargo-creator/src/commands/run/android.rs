use crate::commands::build::{android::AndroidBuildCommand, BuildContext};
use crate::error::Result;
use clap::Clap;
use creator_tools::{commands::android, tools::Bundletool, utils::Config};

#[derive(Clap, Clone, Debug)]
pub struct AndroidRunCommand {
    #[clap(flatten)]
    pub build_command: AndroidBuildCommand,
}

impl AndroidRunCommand {
    pub fn run(&self, config: &Config) -> Result<()> {
        let context = BuildContext::new(config, self.build_command.shared.target_dir.clone())?;
        if self.build_command.aab {
            let apks_path = self.build_command.execute_aab(config, &context)?;
            config.status("Starting run process")?;
            config.status("Installing apks file")?;
            Bundletool.install_apks(&apks_path);
            config.status("Run finished successfully")?;
        } else {
            let (android_manifest, sdk, apk_path) = self.build_command.execute(config, &context)?;
            config.status("Starting run process")?;
            config.status("Installing APK file")?;
            android::install_apk(&sdk, &apk_path)?;
            config.status("Starting APK file")?;
            android::start_apk(&sdk, &android_manifest.package)?;
            config.status("Run finished successfully")?;
        }
        Ok(())
    }
}
