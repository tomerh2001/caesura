use crate::prelude::*;
use qbittorrent_api::add_torrent::AddTorrentOptions;
use qbittorrent_api::get_torrents::{FilterOptions, State};
use qbittorrent_api::{QBittorrentClientFactory, QBittorrentClientOptions};
use tokio::fs::{copy, hard_link};
use tokio::time::{Duration, sleep};

/// Result of injecting a torrent file into a client auto-add directory.
pub(crate) struct TorrentInjectionResult {
    pub target_path: PathBuf,
    pub verb: &'static str,
}

/// Inject a torrent into an auto-add directory, using hard links if configured.
pub(crate) async fn inject_torrent<T: Debug + Display>(
    source_path: &Path,
    target_dir: &Path,
    hard_link_enabled: bool,
    hard_link_action: T,
    copy_action: T,
) -> Result<TorrentInjectionResult, Failure<T>> {
    let source_file_name = source_path
        .file_name()
        .expect("torrent path should have a file name");
    let target_path = target_dir.join(source_file_name);
    let verb = if hard_link_enabled {
        hard_link(source_path, &target_path)
            .await
            .map_err(Failure::wrap_with_path(hard_link_action, &target_path))?;
        "Hard Linked"
    } else {
        copy(source_path, &target_path)
            .await
            .map_err(Failure::wrap_with_path(copy_action, &target_path))?;
        "Copied"
    };
    Ok(TorrentInjectionResult { target_path, verb })
}

/// Inject a torrent and downgrade any injection errors to warnings.
pub(crate) async fn inject_torrent_or_warn<T: Debug + Display>(
    source_path: &Path,
    target_dir: &Path,
    hard_link_enabled: bool,
    hard_link_action: T,
    copy_action: T,
    warnings: &mut Vec<rogue_logging::Error>,
) {
    match inject_torrent(
        source_path,
        target_dir,
        hard_link_enabled,
        hard_link_action,
        copy_action,
    )
    .await
    {
        Ok(result) => {
            trace!(
                "{} {} to {}",
                result.verb.bold(),
                source_path.display(),
                result.target_path.display()
            );
        }
        Err(error) => {
            warn!("{}", error.render());
            warnings.push(error.to_error());
        }
    }
}

/// Inject a torrent via a configured client API.
pub(crate) async fn inject_torrent_with_client<T: Debug + Display + Copy>(
    torrent_path: &Path,
    upload_options: &UploadOptions,
    action: T,
) -> Result<(), Failure<T>> {
    let Some(client) = upload_options.torrent_client else {
        return Ok(());
    };
    match client {
        TorrentClient::Qbittorrent => {
            inject_qbittorrent_torrent(torrent_path, upload_options, action).await
        }
    }
}

async fn inject_qbittorrent_torrent<T: Debug + Display + Copy>(
    torrent_path: &Path,
    upload_options: &UploadOptions,
    action: T,
) -> Result<(), Failure<T>> {
    let (Some(url), Some(username), Some(password)) = (
        &upload_options.torrent_client_url,
        &upload_options.torrent_client_username,
        &upload_options.torrent_client_password,
    ) else {
        return Ok(());
    };
    let add = AddTorrentOptions {
        save_path: upload_options.torrent_client_savepath.clone(),
        category: upload_options.torrent_client_category.clone(),
        tags: upload_options
            .torrent_client_tags
            .as_ref()
            .and_then(|tags| {
                let tags = tags
                    .iter()
                    .map(|tag| tag.trim())
                    .map(ToOwned::to_owned)
                    .filter(|tag| !tag.is_empty())
                    .collect::<Vec<_>>();
                (!tags.is_empty()).then_some(tags)
            }),
        paused: upload_options.torrent_client_paused.then_some(true),
        skip_checking: upload_options.torrent_client_skip_checking.then_some(true),
        ..AddTorrentOptions::default()
    };
    let mut client = QBittorrentClientFactory {
        options: QBittorrentClientOptions {
            host: url.clone(),
            username: username.clone(),
            password: password.clone(),
            ..QBittorrentClientOptions::default()
        },
    }
    .create();
    client.login().await.map_err(Failure::wrap(action))?;
    client
        .add_torrent(add, torrent_path.to_path_buf())
        .await
        .map_err(Failure::wrap(action))?
        .get_result("add_torrent")
        .map_err(Failure::wrap(action))?;
    let info_hash = match TorrentReader::execute(torrent_path).await {
        Ok(torrent) => torrent.info_hash(),
        Err(error) => {
            warn!(
                "{} to verify injected torrent state in qBittorrent: {}",
                "Failed".bold(),
                error.render()
            );
            trace!(
                "{} {} via qBittorrent API",
                "Injected".bold(),
                torrent_path.display()
            );
            return Ok(());
        }
    };
    sleep(Duration::from_millis(500)).await;
    let missing_files = client
        .get_torrents(FilterOptions {
            hashes: Some(info_hash),
            ..FilterOptions::default()
        })
        .await
        .map_err(Failure::wrap(action))?
        .get_result("get_torrents")
        .map_err(Failure::wrap(action))?
        .first()
        .is_some_and(|torrent| torrent.state == State::MissingFiles);
    if missing_files {
        warn!(
            "Torrent injected into qBittorrent but data files are missing for seeding. \
Set `torrent_client_savepath` to a path qBittorrent can access and ensure data files are present there."
        );
    }
    trace!(
        "{} {} via qBittorrent API",
        "Injected".bold(),
        torrent_path.display()
    );
    Ok(())
}
