use std::sync::Arc;

use anyhow::Context as _;
use futures::TryStreamExt as _;
use tracing::Instrument;

use crate::brioche::{
    value::{CompleteValue, Directory, DirectoryError, File, Meta, UnpackValue, WithMeta},
    Brioche,
};

#[tracing::instrument(skip(brioche, unpack), fields(file_value = %unpack.file.hash(), archive = ?unpack.archive, compression = ?unpack.compression))]
pub async fn resolve_unpack(
    brioche: &Brioche,
    meta: &Arc<Meta>,
    unpack: UnpackValue,
) -> anyhow::Result<Directory> {
    let file = super::resolve(brioche, *unpack.file).await?;
    let CompleteValue::File(File { data: blob_id, .. }) = file.value else {
        anyhow::bail!("expected archive to be a file");
    };

    tracing::debug!(%blob_id, archive = ?unpack.archive, compression = ?unpack.compression, "starting unpack");

    let job_id = brioche.reporter.add_job(crate::reporter::NewJob::Unpack);

    let archive_path = crate::brioche::blob::blob_path(brioche, blob_id);
    let archive_file = tokio::fs::File::open(&archive_path).await?;
    let uncompressed_archive_size = archive_file.metadata().await?.len();
    let archive_file = tokio::io::BufReader::new(archive_file);

    let decompressed_archive_file = unpack.compression.decompress(archive_file);

    let mut archive = tokio_tar::Archive::new(decompressed_archive_file);
    let mut archive_entries = archive.entries()?;
    let mut directory = Directory::default();

    let save_blobs_future = async {
        while let Some(archive_entry) = archive_entries.try_next().await? {
            let entry_path = bstr::BString::new(archive_entry.path_bytes().into_owned());
            let entry_mode = archive_entry.header().mode()?;

            let position = archive_entry.raw_file_position();
            let estimated_progress = position as f64 / (uncompressed_archive_size as f64).max(1.0);
            let progress_percent = (estimated_progress * 100.0).min(99.0) as u8;
            brioche.reporter.update_job(
                job_id,
                crate::reporter::UpdateJob::Unpack { progress_percent },
            );

            let entry_value = match archive_entry.header().entry_type() {
                tokio_tar::EntryType::Regular => {
                    let entry_blob_id = crate::brioche::blob::save_blob(
                        brioche,
                        archive_entry,
                        crate::brioche::blob::SaveBlobOptions::new(),
                    )
                    .await?;
                    let executable = entry_mode & 0o100 != 0;

                    CompleteValue::File(File {
                        data: entry_blob_id,
                        executable,
                        resources: Directory::default(),
                    })
                }
                tokio_tar::EntryType::Symlink => {
                    let link_name = archive_entry.link_name_bytes().with_context(|| {
                        format!(
                            "unsupported tar archive: no link name for symlink entry at {}",
                            entry_path
                        )
                    })?;

                    CompleteValue::Symlink {
                        target: link_name.into_owned().into(),
                    }
                }
                tokio_tar::EntryType::Link => {
                    let link_name = archive_entry.link_name_bytes().with_context(|| {
                        format!(
                            "unsupported tar archive: no link name for hardlink entry at {}",
                            entry_path
                        )
                    })?;
                    let linked_entry = directory.get(link_name.as_ref())?.with_context(|| {
                        format!(
                            "unsupported tar archive: could not find target for link entry at {}",
                            entry_path
                        )
                    })?;

                    linked_entry.value.clone()
                }
                tokio_tar::EntryType::Directory => CompleteValue::Directory(Directory::default()),
                other => {
                    anyhow::bail!(
                        "unsupported tar archive: unsupported entry type {:?} at {}",
                        other,
                        entry_path
                    );
                }
            };

            match directory.insert(&entry_path, WithMeta::new(entry_value, meta.clone())) {
                Ok(_) => {}
                Err(DirectoryError::EmptyPath { .. }) => {
                    tracing::debug!("skipping empty path in tar archive");
                    // Tarfiles can have entries pointing to the root path, which
                    // we can safely ignore
                }
                Err(error) => {
                    return Err(error.into());
                }
            }
        }

        brioche.reporter.update_job(
            job_id,
            crate::reporter::UpdateJob::Unpack {
                progress_percent: 100,
            },
        );

        anyhow::Ok(())
    }
    .instrument(tracing::info_span!("save_blobs"));

    save_blobs_future.await?;

    Ok(directory)
}