use crate::prelude::*;
use gazelle_api::{GazelleClientTrait, GroupResponse};
use serde::Serialize;
use tokio::fs::rename;

/// Result of a publish operation.
#[derive(Debug)]
pub(crate) struct PublishSuccess {
    pub group_id: Option<u32>,
    pub torrent_id: Option<u32>,
    pub warnings: Vec<rogue_logging::Error>,
}

impl PublishSuccess {
    #[must_use]
    pub fn permalink(&self, indexer_url: &str) -> Option<String> {
        self.group_id
            .zip(self.torrent_id)
            .map(|(group_id, torrent_id)| get_permalink(indexer_url, group_id, torrent_id))
    }

    #[must_use]
    pub fn next_transcode_command(&self) -> Option<String> {
        self.torrent_id
            .map(|torrent_id| format!("caesura transcode {torrent_id}"))
    }

    #[must_use]
    pub fn next_upload_command(&self) -> Option<String> {
        self.torrent_id
            .map(|torrent_id| format!("caesura upload {torrent_id}"))
    }
}

/// Publish a source FLAC torrent from a local directory using a manifest.
#[injectable]
pub(crate) struct PublishCommand {
    arg: Ref<PublishArg>,
    shared_options: Ref<SharedOptions>,
    publish_seeding_options: Ref<PublishSeedingOptions>,
    batch_options: Ref<BatchOptions>,
    upload_options: Ref<UploadOptions>,
    copy_options: Ref<CopyOptions>,
    api: Ref<Box<dyn GazelleClientTrait + Send + Sync>>,
    paths: Ref<PathManager>,
}

impl PublishCommand {
    /// Execute `publish` from CLI.
    pub(crate) async fn execute_cli(&self) -> Result<bool, Failure<PublishAction>> {
        let manifest = PublishManifest::read(&self.arg.publish_path)
            .map_err(Failure::wrap(PublishAction::ParseManifest))?;
        manifest.validate().map_err(|errors| {
            Failure::new(
                PublishAction::ValidateManifest,
                PublishValidationErrors(errors),
            )
        })?;

        let result = self.execute(&manifest).await?;
        if !result.warnings.is_empty() {
            trace!(
                "Publish completed with {} warning(s)",
                result.warnings.len()
            );
        }
        if let (Some(group_id), Some(torrent_id)) = (result.group_id, result.torrent_id) {
            info!("{} source FLAC", "Published".bold());
            if let Some(link) = result.permalink(&self.shared_options.indexer_url) {
                info!("{link}");
            }
            if let Some(next_transcode) = result.next_transcode_command() {
                info!("Next: {next_transcode}");
            }
            if let Some(next_upload) = result.next_upload_command() {
                info!("Next: {next_upload}");
            }
            trace!("Published group_id={group_id} torrent_id={torrent_id}");
        } else {
            info!("{} source FLAC as this is a dry run", "Skipped".bold());
        }
        Ok(true)
    }

    /// Execute `publish` against an already-parsed manifest.
    pub(crate) async fn execute(
        &self,
        manifest: &PublishManifest,
    ) -> Result<PublishSuccess, Failure<PublishAction>> {
        let mut warnings = Vec::new();
        let indexer = self.shared_options.indexer_lowercase();
        if indexer != "red" {
            return Err(Failure::new(
                PublishAction::ValidateTracker,
                PublishError::UnsupportedIndexer { indexer },
            ));
        }

        let torrent_path = manifest.torrent_path.clone().unwrap_or_else(|| {
            let source_name = manifest
                .source_path
                .file_name()
                .expect("source_path should have a file name")
                .to_string_lossy()
                .to_string();
            self.paths
                .get_output_dir()
                .join(format!("{source_name}.{indexer}.source.torrent"))
        });
        let dry_run = self.arg.dry_run;

        TorrentCreator::create(
            &manifest.source_path,
            &torrent_path,
            self.shared_options.announce_url.clone(),
            indexer,
        )
        .await
        .map_err(Failure::wrap(PublishAction::CreateTorrent))?;
        let seeding_source = self
            .prepare_seeding_source(manifest, &torrent_path, dry_run, &mut warnings)
            .await?;

        let source_title = manifest.group.source_title();
        let release_description = Self::create_release_description(
            &seeding_source,
            &manifest.release_desc,
            &source_title,
        );

        match &manifest.group {
            PublishGroup::NewGroup(new_group) => {
                self.publish_new_group(
                    new_group,
                    &torrent_path,
                    &seeding_source,
                    &source_title,
                    release_description,
                    dry_run,
                    warnings,
                )
                .await
            }
            PublishGroup::ExistingGroup(existing_group) => {
                self.publish_existing_group(
                    existing_group,
                    &torrent_path,
                    &seeding_source,
                    &source_title,
                    release_description,
                    dry_run,
                    warnings,
                )
                .await
            }
        }
    }

    async fn prepare_seeding_source(
        &self,
        manifest: &PublishManifest,
        torrent_path: &Path,
        dry_run: bool,
        warnings: &mut Vec<rogue_logging::Error>,
    ) -> Result<PathBuf, Failure<PublishAction>> {
        let source_dir_name = manifest
            .source_path
            .file_name()
            .expect("source_path should have a file name");
        let seeding_destination = self
            .shared_options
            .content
            .first()
            .expect("content should contain at least one directory")
            .join(source_dir_name);
        let source_already_staged = manifest.source_path == seeding_destination;
        if dry_run {
            if source_already_staged {
                trace!(
                    "{} source staging because source already at destination: {}",
                    "Skipping".bold(),
                    seeding_destination.display()
                );
            } else if self.publish_seeding_options.move_source {
                trace!(
                    "Dry run: source would be moved from {} to {}",
                    manifest.source_path.display(),
                    seeding_destination.display()
                );
            } else {
                trace!(
                    "Dry run: source would be hard linked from {} to {}",
                    manifest.source_path.display(),
                    seeding_destination.display()
                );
            }
            if self.publish_seeding_options.copy_torrent_to.is_some() {
                if self.copy_options.hard_link {
                    trace!("Dry run: torrent would be hard linked to autoadd directory");
                } else {
                    trace!("Dry run: torrent would be copied to autoadd directory");
                }
            }
            return Ok(manifest.source_path.clone());
        }

        let seeding_source = if source_already_staged {
            trace!(
                "{} source staging because source already at destination: {}",
                "Skipping".bold(),
                seeding_destination.display()
            );
            manifest.source_path.clone()
        } else {
            if seeding_destination.exists() {
                return Err(Failure::new(
                    PublishAction::StageSource,
                    IoError::new(
                        ErrorKind::AlreadyExists,
                        "seeding destination already exists",
                    ),
                )
                .with_path(&seeding_destination));
            }
            if self.publish_seeding_options.move_source {
                trace!(
                    "{} source from {} to {}",
                    "Moving".bold(),
                    manifest.source_path.display(),
                    seeding_destination.display()
                );
                rename(&manifest.source_path, &seeding_destination)
                    .await
                    .map_err(Failure::wrap_with_path(
                        PublishAction::StageSource,
                        &seeding_destination,
                    ))?;
            } else {
                trace!(
                    "{} source from {} to {}",
                    "Hard Linking".bold(),
                    manifest.source_path.display(),
                    seeding_destination.display()
                );
                copy_dir(&manifest.source_path, &seeding_destination, true)
                    .await
                    .map_err(Failure::wrap(PublishAction::StageSource))?;
            }
            seeding_destination
        };

        self.verify_seed_content(torrent_path, &seeding_source)
            .await?;

        if let Some(torrent_dir) = &self.publish_seeding_options.copy_torrent_to {
            inject_torrent_or_warn(
                torrent_path,
                torrent_dir,
                self.copy_options.hard_link,
                PublishAction::InjectTorrent,
                PublishAction::InjectTorrent,
                warnings,
            )
            .await;
        }
        if let Err(error) = inject_torrent_with_client(
            torrent_path,
            &self.upload_options,
            PublishAction::InjectTorrentClient,
        )
        .await
        {
            warn!("{}", error.render());
            warnings.push(error.to_error());
        }

        Ok(seeding_source)
    }

    async fn publish_new_group(
        &self,
        new_group: &PublishNewGroup,
        torrent_path: &Path,
        seeding_source: &Path,
        source_title: &str,
        release_description: String,
        dry_run: bool,
        mut warnings: Vec<rogue_logging::Error>,
    ) -> Result<PublishSuccess, Failure<PublishAction>> {
        let form = PublishManifest::to_new_source_form(
            new_group,
            torrent_path.to_path_buf(),
            release_description,
        );
        if dry_run {
            info!("{} upload as this is a dry run", "Skipping".bold());
            info!("{} data for source upload:", "Upload".bold());
            info!("\n{form}");
            return Ok(PublishSuccess {
                group_id: None,
                torrent_id: None,
                warnings,
            });
        }
        let response = self
            .api
            .upload_new_source(form)
            .await
            .map_err(Failure::wrap(PublishAction::UploadNewSource))?;
        self.execute_post_upload_hook(
            source_title,
            seeding_source,
            torrent_path,
            response.group_id,
            response.torrent_id,
            &mut warnings,
        )
        .await;
        Ok(PublishSuccess {
            group_id: Some(response.group_id),
            torrent_id: Some(response.torrent_id),
            warnings,
        })
    }

    async fn publish_existing_group(
        &self,
        existing_group: &PublishExistingGroup,
        torrent_path: &Path,
        seeding_source: &Path,
        source_title: &str,
        release_description: String,
        dry_run: bool,
        mut warnings: Vec<rogue_logging::Error>,
    ) -> Result<PublishSuccess, Failure<PublishAction>> {
        let form = PublishManifest::to_existing_group_form(
            existing_group,
            torrent_path.to_path_buf(),
            release_description,
        );
        if dry_run {
            info!("{} upload as this is a dry run", "Skipping".bold());
            info!("{} data for source upload:", "Upload".bold());
            info!("\n{form}");
            return Ok(PublishSuccess {
                group_id: None,
                torrent_id: None,
                warnings,
            });
        }
        let group = self
            .api
            .get_torrent_group(existing_group.group_id)
            .await
            .map_err(Failure::wrap(PublishAction::GetTorrentGroup))?;
        if Self::is_duplicate_existing_group_source(existing_group, &group)? {
            return Err(Failure::new(
                PublishAction::CheckDuplicate,
                PublishError::DuplicateSource,
            ));
        }

        let response = self
            .api
            .upload_torrent(form)
            .await
            .map_err(Failure::wrap(PublishAction::UploadExistingGroup))?;
        self.execute_post_upload_hook(
            source_title,
            seeding_source,
            torrent_path,
            response.group_id,
            response.torrent_id,
            &mut warnings,
        )
        .await;
        Ok(PublishSuccess {
            group_id: Some(response.group_id),
            torrent_id: Some(response.torrent_id),
            warnings,
        })
    }

    async fn execute_post_upload_hook(
        &self,
        source_name: &str,
        seeding_source: &Path,
        torrent_path: &Path,
        group_id: u32,
        torrent_id: u32,
        warnings: &mut Vec<rogue_logging::Error>,
    ) {
        let Some(hook_path) = &self.batch_options.post_upload_hook else {
            return;
        };
        let payload = self.create_hook_payload(
            source_name,
            seeding_source,
            torrent_path,
            group_id,
            torrent_id,
        );
        if let Err(error) = execute_yaml_hook(
            hook_path,
            &payload,
            PublishAction::SerializeHookPayload,
            PublishAction::WriteHookPayload,
            PublishAction::ExecuteHook,
        )
        .await
        {
            warn!("{}", error.render());
            warnings.push(error.to_error());
        }
    }

    fn create_hook_payload(
        &self,
        source_name: &str,
        seeding_source: &Path,
        torrent_path: &Path,
        group_id: u32,
        torrent_id: u32,
    ) -> PublishHookPayload {
        PublishHookPayload {
            torrent_id: Some(torrent_id),
            group_id: Some(group_id),
            permalink: Some(get_permalink(
                &self.shared_options.indexer_url,
                group_id,
                torrent_id,
            )),
            source_name: source_name.to_owned(),
            source_path: seeding_source.to_string_lossy().to_string(),
            transcode_path: seeding_source.to_string_lossy().to_string(),
            torrent_path: torrent_path.to_string_lossy().to_string(),
        }
    }

    fn is_duplicate_existing_group_source(
        existing_group: &PublishExistingGroup,
        group: &GroupResponse,
    ) -> Result<bool, Failure<PublishAction>> {
        let probe_torrent = gazelle_api::Torrent {
            media: existing_group.media.to_string(),
            format: existing_group.format.clone(),
            encoding: existing_group.bitrate.clone(),
            remaster_year: Some(existing_group.remaster_year),
            remaster_title: existing_group.remaster_title.clone(),
            remaster_record_label: existing_group.remaster_record_label.clone(),
            remaster_catalogue_number: existing_group.remaster_catalogue_number.clone(),
            ..gazelle_api::Torrent::default()
        };
        let existing = ExistingFormatProvider::get(&probe_torrent, &group.torrents);
        let source_format = ExistingFormat::from_torrent(&probe_torrent).ok_or_else(|| {
            Failure::new(
                PublishAction::CheckDuplicate,
                PublishError::UnsupportedSourceFormat,
            )
        })?;
        Ok(existing.contains(&source_format))
    }

    #[allow(clippy::uninlined_format_args)]
    pub(crate) fn create_release_description(
        source_path: &Path,
        release_notes: &str,
        source_title: &str,
    ) -> String {
        let mut lines = vec![format!(
            "Published and uploaded with [url={}][b]{}[/b] {}[/url]",
            APP_HOMEPAGE,
            APP_NAME,
            app_version_or_describe()
        )];
        lines.push(format!("[pad=0|10|0|20]Source[/pad] {source_title}"));
        let release_notes = release_notes.trim();
        if !release_notes.is_empty() {
            lines.push(format!("[pad=0|10|0|21]Notes[/pad] {release_notes}"));
        }
        append_inspect_sections(
            &mut lines,
            source_path,
            "Unable to add source details to publish description",
        );
        to_quote_blocks(lines)
    }

    pub(crate) async fn verify_seed_content(
        &self,
        torrent_path: &Path,
        seeding_source: &Path,
    ) -> Result<(), Failure<PublishAction>> {
        let verification = TorrentVerifier::execute(torrent_path, seeding_source)
            .await
            .map_err(Failure::wrap(PublishAction::VerifySeedContent))?;
        if let Some(issue) = verification {
            return Err(Failure::new(
                PublishAction::VerifySeedContent,
                PublishError::SeedContentVerification {
                    issue: issue.to_string(),
                },
            )
            .with_path(seeding_source));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct PublishHookPayload {
    torrent_id: Option<u32>,
    group_id: Option<u32>,
    permalink: Option<String>,
    source_name: String,
    source_path: String,
    transcode_path: String,
    torrent_path: String,
}
