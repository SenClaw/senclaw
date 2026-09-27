use anyhow::Result;
use std::path::Path;
use tokio::fs::{self as tokio_fs, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Reads the entire contents of a file into a string asynchronously,
/// similar to Node.js `fs.readFile(path, 'utf8')`.
pub async fn read_file_utf8<P: AsRef<Path>>(path: P) -> Result<String> {
    let mut file = File::open(path).await?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).await?;
    Ok(contents)
}

/// Writes a string to a file asynchronously,
/// similar to Node.js `fs.writeFile(path, data, 'utf8')`.
pub async fn write_file_utf8<P: AsRef<Path>>(path: P, data: &str) -> Result<()> {
    let mut file = File::create(path).await?;
    file.write_all(data.as_bytes()).await?;
    Ok(())
}

/// Checks if a path exists.
pub async fn exists<P: AsRef<Path>>(path: P) -> bool {
    tokio_fs::try_exists(path).await.unwrap_or(false)
}
