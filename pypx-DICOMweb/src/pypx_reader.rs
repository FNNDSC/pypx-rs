//! Reads data from a pypx-organized directory, presenting it in "DICOMweb format."

use crate::constants;
use crate::dicom::dicomfile2json;
use crate::errors::{FileError, PypxBaseNotADir, ReadDirError};
use crate::json_files::{read_1member_json_file, read_json_file};
use crate::translate::{series_meta_to_dicomweb, study_meta_to_dicomweb};
use futures::{pin_mut, StreamExt};
use pypx::{InstanceData, StudyDataMeta, StudyDataSeriesMeta};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio_stream::wrappers::ReadDirStream;
use tracing::{event, Level};

/// Creates a closure suitable for use by [StreamExt::filter_map]
/// which filters paths by extension.
///
/// Using a macro is necessary because returning an async closure from a function
/// is currently unstable: https://github.com/rust-lang/rust/issues/62290
macro_rules! select_files_by_extension {
    ($extension:expr) => {
        |path| async move {
            if path.is_file() && path.file_name()?.to_str()?.ends_with($extension) {
                Some(path)
            } else {
                None
            }
        }
    };
}

/// Reader of a pypx-organized directory of DICOM and JSON files.
pub struct PypxReader {
    study_data_dir: PathBuf,
    series_data_dir: PathBuf,
    data_dir: PathBuf,

    /// Path where the data directory is mounted for the repacker
    /// (`rx-repack`, which is called by `storescp`)
    repack_data_dir_mountpath: PathBuf,
}

impl PypxReader {
    /// Instantiate a [PypxReader], checking to make sure the right directories exist
    /// (`log/studyData`, `log/seriesData`).
    pub fn new(
        log_dir: &Path,
        data_dir: PathBuf,
        repack_data_dir_mountpath: PathBuf,
    ) -> Result<Self, PypxBaseNotADir> {
        let study_data_dir = log_dir.join("studyData");
        let series_data_dir = log_dir.join("seriesData");

        let all = [&study_data_dir, &series_data_dir];
        if !all.iter().all(|p| p.is_dir()) {
            Err(PypxBaseNotADir(study_data_dir))
        } else {
            Ok(Self {
                study_data_dir,
                series_data_dir,
                data_dir,
                repack_data_dir_mountpath,
            })
        }
    }

    /// Find study metadata from the pypx-organized filesystem.
    /// Returns data in DICOMweb's response schema.
    pub async fn query_studies(
        &self,
        query: &HashMap<String, String>,
        limit: usize,
    ) -> Result<Vec<Value>, FileError> {
        if limit == 0 {
            return Ok(vec![]);
        }
        // TODO add PatientName to the data
        let studies = if let Some(study_instance_uid) = query.get("StudyInstanceUID") {
            flatten_notfound_error(self.get_study(study_instance_uid).await)?
        } else {
            self.ls_studies(query, limit).await
        };
        let dicomweb_response = studies.iter().map(study_meta_to_dicomweb).collect();
        Ok(dicomweb_response)
    }

    /// Find studies matching a given filter, returning study metadata as a DICOMweb response.
    async fn ls_studies<'a>(
        &'a self,
        query: &'a HashMap<String, String>,
        limit: usize,
    ) -> Vec<StudyDataMeta> {
        let path = &self.study_data_dir;
        let read_dir = tokio::fs::read_dir(path)
            .await
            .map_err(|e| ReadDirError(path.to_path_buf(), e.kind()))
            // assuming study dir exists, we checked it in Self::new()
            .unwrap_or_else(|_| panic!("{:?} is not a directory", &self.study_data_dir));
        let stream = ReadDirStream::new(read_dir)
            .filter_map(report_then_discard_error)
            .map(|entry| entry.path())
            .filter_map(select_files_by_extension!("-meta.json"))
            .map(read_study_meta_json)
            .buffer_unordered(4)
            .filter_map(report_then_discard_error)
            .filter_map(|study| study_matches_wrapper(study, query));

        // causes "error: higher-ranked lifetime error"
        // stream
        //     .zip(futures::stream::iter(0..limit))
        //     .filter_map(|(x, _)| async move { Some(x) })
        //     .collect()
        //     .await

        pin_mut!(stream);
        let mut data = Vec::with_capacity(limit.min(200));
        while let Some(next) = stream.next().await {
            data.push(next);
            if data.len() >= limit {
                return data;
            }
        }
        data
    }

    /// Get a single study and its metadata.
    async fn get_study(&self, study_instance_uid: &str) -> Result<StudyDataMeta, FileError> {
        let file = self.study_meta_file_for(study_instance_uid);
        let result: Result<StudyDataMeta, _> = read_study_meta_json(file).await;
        result
    }

    /// List the series of a study.
    pub async fn get_series(&self, study_instance_uid: &str) -> Result<Vec<Value>, ReadDirError> {
        let path = self.series_meta_dir_of(study_instance_uid);
        let read_dir = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| ReadDirError(path, e.kind()))?;
        let datas = ReadDirStream::new(read_dir)
            .filter_map(report_then_discard_error)
            .map(|entry| entry.path())
            .filter_map(select_files_by_extension!("-meta.json"))
            .map(|path| self.get_series_data(path))
            .buffer_unordered(4)
            .filter_map(report_then_discard_error)
            .collect()
            .await;
        Ok(datas)
    }

    /// Serialize all DICOMs of a series into JSON.
    pub async fn get_series_dicomweb_metadata(
        &self,
        study_instance_uid: &str,
        series_instance_uid: &str,
    ) -> Result<Vec<Value>, FileError> {
        let dcms = self
            .ls_dcm(study_instance_uid, series_instance_uid)
            .await?
            .map(dicomfile2json)
            .buffer_unordered(4)
            .filter_map(report_then_discard_error)
            .collect()
            .await;
        Ok(dcms)
    }

    /// Get `FSlocation` from the JSON file which describes a DICOM instance file.
    pub async fn get_instance_fslocation(
        &self,
        series_instance_uid: &str,
        sop_instance_uid: &str,
    ) -> Result<PathBuf, FileError> {
        let series_dir = self.instances_json_dir_for(series_instance_uid);
        let find = self
            .find_instance_meta_file(&series_dir, sop_instance_uid)
            .await?;
        if let Some(path) = find {
            let instance_data: InstanceData = read_1member_json_file(&path).await?;
            instance_data
                .imageObj
                .into_values()
                .next()
                .ok_or_else(|| {
                    FileError::Malformed(
                        path.to_path_buf(),
                        "Value for JSON key `imageObj` is an empty object".to_string(),
                        None,
                    )
                })
                .and_then(|o| {
                    self.change_data_mount_path(o.FSlocation.as_ref())
                        .ok_or(FileError::Malformed(
                            path,
                            format!(
                                "FSlocation={} is not relative to PYPX_REPACK_DATA_MOUNTPOINT={:?}",
                                o.FSlocation, self.repack_data_dir_mountpath
                            ),
                            None,
                        ))
                })
        } else {
            Err(FileError::NotFound(
                series_dir.join(format!("????-{sop_instance_uid}.dcm.json")),
            ))
        }
    }

    // Helper functions for getting information from files and directories
    // --------------------------------------------------------------------------------

    /// Given a path `log/studyData/XXX-series/X-meta.json`, produce the metadata of the
    /// corresponding series including `NumberOfSeriesRelatedInstances`.
    async fn get_series_data(&self, path: PathBuf) -> Result<Value, FileError> {
        let data: StudyDataSeriesMeta = read_1member_json_file(&path).await?;
        let series_instance_uid = data.SeriesInstanceUID.as_ref();
        let num_instances = self.count_instances(series_instance_uid).await.unwrap_or(0);
        let value = series_meta_to_dicomweb(&data, num_instances);
        Ok::<Value, FileError>(value)
    }

    /// Count the number of DICOM instances in the specified series.
    async fn count_instances(&self, series_instance_uid: &str) -> Result<usize, std::io::Error> {
        let path = self.instances_json_dir_for(series_instance_uid);
        let read_dir = tokio::fs::read_dir(path).await?;
        let count = ReadDirStream::new(read_dir)
            .filter_map(report_then_discard_error)
            .map(|entry| entry.path())
            .filter_map(select_files_by_extension!(".dcm.json"))
            .count()
            .await;
        Ok(count)
    }

    /// Iterate over the DICOM files of a series.
    async fn ls_dcm(
        &self,
        study_instance_uid: &str,
        series_instance_uid: &str,
    ) -> Result<impl futures::Stream<Item = PathBuf>, FileError> {
        let series_meta_file =
            self.studydata_series_meta_file_for(study_instance_uid, series_instance_uid);
        let series_meta: StudyDataSeriesMeta = read_1member_json_file(&series_meta_file).await?;
        let series_data_dir = self
            .change_data_mount_path(series_meta.SeriesBaseDir.as_ref())
            .ok_or_else(|| {
                FileError::Malformed(
                    series_meta_file.to_path_buf(),
                    format!(
                        "SeriesBaseDir={} is not relative to PYPX_REPACK_DATA_MOUNTPOINT={:?}",
                        series_meta.SeriesBaseDir, self.repack_data_dir_mountpath
                    ),
                    None,
                )
            })?;
        let read_dir = tokio::fs::read_dir(&series_data_dir)
            .await
            .map_err(|error| {
                FileError::Malformed(
                    series_meta_file,
                    format!(
                        "Cannot read SeriesBaseDir directory: {:?}",
                        &series_data_dir
                    ),
                    Some(error.into()),
                )
            })?;
        let stream = ReadDirStream::new(read_dir)
            .filter_map(report_then_discard_error)
            .map(|entry| entry.path())
            .filter_map(select_files_by_extension!(".dcm"));
        Ok(stream)
    }

    /// Change a path from the repacker's filesystem to the filesystem that is visible
    /// to this program.
    fn change_data_mount_path<P: AsRef<Path>>(&self, path: P) -> Option<PathBuf> {
        pathdiff::diff_paths(path.as_ref(), &self.repack_data_dir_mountpath)
            .map(|p| self.data_dir.join(p))
            .or_else(|| {
                event!(
                    Level::ERROR,
                    "{:?} is not relative to PYPX_DATA_DIR={:?}",
                    path.as_ref(),
                    self.repack_data_dir_mountpath
                );
                None
            })
    }

    /// Find a file under `series_dir` called `????-{sop_instance_uid}.dcm.json`
    async fn find_instance_meta_file(
        &self,
        series_dir: &Path,
        sop_instance_uid: &str,
    ) -> Result<Option<PathBuf>, FileError> {
        let read_dir = tokio::fs::read_dir(series_dir)
            .await
            .map_err(|e| FileError::ParentDirNotReadable(series_dir.to_path_buf(), e.kind()))?;
        let filter = ReadDirStream::new(read_dir)
            .filter_map(report_then_discard_error)
            .map(|entry| entry.path())
            .filter_map(select_files_by_extension!(".dcm.json"))
            .filter_map(|path| async move {
                let file_sop_instance_uid = path
                    .file_name()
                    .and_then(|file_name| file_name.to_str())
                    .map(|file_name| {
                        &file_name[("0000-".len())..(file_name.len() - ".dcm.json".len())]
                    });
                if Some(sop_instance_uid) == file_sop_instance_uid {
                    Some(path)
                } else {
                    None
                }
            });
        pin_mut!(filter);
        let first = filter.next().await;
        Ok(first)
    }

    // Helper functions related to the pypx organization of files
    // --------------------------------------------------------------------------------

    fn study_meta_file_for(&self, study_instance_uid: &str) -> PathBuf {
        let name = format!("{study_instance_uid}-meta.json");
        self.study_data_dir.join(name)
    }

    fn series_meta_dir_of(&self, study_instance_uid: &str) -> PathBuf {
        let name = format!("{study_instance_uid}-series");
        self.study_data_dir.join(name)
    }

    fn instances_json_dir_for(&self, series_instance_uid: &str) -> PathBuf {
        let name = format!("{series_instance_uid}-img");
        self.series_data_dir.join(name)
    }

    fn studydata_series_meta_file_for(
        &self,
        study_instance_uid: &str,
        series_instance_uid: &str,
    ) -> PathBuf {
        let name = format!("{series_instance_uid}-meta.json");
        self.series_meta_dir_of(study_instance_uid).join(name)
    }
}

/// A wrapper to handle a bug in `rx-repack` which was fixed in version 1.0.3
/// https://github.com/FNNDSC/pypx-listener/commit/b453fb375f180dbad6ebd9df27966b5ff0ac484e
async fn read_study_meta_json(path: PathBuf) -> Result<StudyDataMeta<'static>, FileError> {
    match read_1member_json_file(&path).await {
        Ok(study) => Ok(study),
        Err(error) => {
            if matches!(error, FileError::Malformed(..)) {
                read_json_file(&path).await.map(|study| {
                    event!(
                        Level::WARN,
                        "File is affected by rx-repack bug, please fix by \
                        repacking the DICOM file using rx-repack v1.0.3 or greater. {:?}",
                        path
                    );
                    study
                })
            } else {
                Err(error)
            }
        }
    }
}

/// A wrapper for [study_matches] with lifetime annotations
/// so that it may be used with [StreamExt::filter_map].
async fn study_matches_wrapper<'a>(
    study: StudyDataMeta<'a>,
    query: &'a HashMap<String, String>,
) -> Option<StudyDataMeta<'a>> {
    if study_matches(&study, query) {
        Some(study)
    } else {
        None
    }
}

/// Returns `true` if the given study matches the given query.
fn study_matches(study: &StudyDataMeta, query: &HashMap<String, String>) -> bool {
    if let Some(patient_id) = query.get(constants::PATIENT_ID) {
        if patient_id != study.PatientID.as_ref() {
            return false;
        }
    }
    true
}

fn flatten_notfound_error<T>(result: Result<T, FileError>) -> Result<Vec<T>, FileError> {
    match result {
        Ok(value) => Ok(vec![value]),
        Err(error) => match error {
            FileError::NotFound(_path) => Ok(vec![]),
            _ => Err(error),
        },
    }
}

async fn report_then_discard_error<T, E: std::error::Error>(result: Result<T, E>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            event!(Level::ERROR, "{:?}", error);
            None
        }
    }
}
