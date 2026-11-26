use crate::archive::ArchiveFormats;

// Native extensions are composed into this build. A format crate only needs
// the public archive API and can be registered here independently of the app.
pub(crate) static BUNDLED: ArchiveFormats = ArchiveFormats::new(&[
    #[cfg(feature = "zip-extension")]
    &dotdir_zip::ZIP,
    #[cfg(feature = "sevenz-extension")]
    &dotdir_sevenz::SEVEN_ZIP,
    #[cfg(feature = "tar-extension")]
    &dotdir_tar::TAR,
]);
