use std::io::{Error as IoError, ErrorKind as IoErrorKind, Read};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use async_compression::futures::bufread::GzipDecoder;
use async_ssh2_tokio::Client;
use futures::{AsyncReadExt, StreamExt, TryStreamExt};
use http_body_util::BodyStream;
use log::{debug, error, info, warn};
use russh_sftp::client::SftpSession;
use semver::Version;
use tar::Archive;
use tokio::fs::File;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::{Error, ErrorKind};
use crate::Result;

const WINDOWS_TARGET: &str = include_str!("target.bat");
const UNIX_TARGET: &str = include_str!("target.sh");
const VERSION: &str = env!("CARGO_PKG_VERSION");

enum Os {
    Windows,
    UnixLike,
}

pub(crate) async fn installer(
    client: Client,
    mut destination: PathBuf,
    custom_binary: Option<PathBuf>,
    mut overwrite: bool,
) -> Result<()> {
    let os = identify_os(&client).await?; // identify the remote os

    // loop until the destination is a file
    loop {
        match is_dir(&client, &os, &destination).await {
            // path exists and is a directory
            Ok(true) => {
                // append the binary name to the destination if it is a directory
                match os {
                    Os::Windows => destination.push("cccp.exe"),
                    Os::UnixLike => destination.push("cccp"),
                }

                debug!("appended binary name to destination: {:?}", destination);
            }
            // path exists and is a file
            Ok(false) => {
                if !overwrite {
                    // async stdout prompt
                    let mut stdout = stdout();
                    stdout
                        .write_all(b"file already exists, overwrite? [y/N] ")
                        .await?;
                    stdout.flush().await?;

                    let mut input = String::new(); // buffer for input
                    let mut stdin = BufReader::new(stdin()); // stdin reader

                    stdin.read_line(&mut input).await?; // read input into buffer
                    input.make_ascii_lowercase(); // convert to lowercase

                    overwrite = input.starts_with('y'); // update overwrite
                }

                if overwrite {
                    break;
                } else {
                    return Ok(());
                }
            }
            Err(error) => match error.kind.deref() {
                // if the file does not exist, that is fine because it will be created
                ErrorKind::FileNotFound => break,
                _ => return Err(error),
            },
        }
    }

    if let Some(file_path) = custom_binary {
        debug!("using custom binary {}", file_path.display());

        let mut file = File::open(file_path).await?;
        let mut content = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut content).await?;

        transfer_binary(&client, &content, &destination, &os).await?;

        return Ok(());
    }

    let triple = get_triple(&client, &os).await?; // get the target triple

    // initialize octocrab
    let octocrab = octocrab::instance();

    // get the latest release
    let release = octocrab
        .repos("chanderlud", "cccp")
        .releases()
        .get_latest()
        .await?;

    info!("installing cccp-{}-{}", release.tag_name, triple);

    // cccp tags are always prefixed with a v
    let tag_version = release.tag_name.strip_prefix('v').unwrap();

    // check if the local version is out of date
    if Version::parse(VERSION)? < Version::parse(tag_version)? {
        warn!(
            "the local version of cccp is out of date [v{} < {}]",
            VERSION, release.tag_name
        );
    }

    for asset in &release.assets {
        if asset.name.contains(&triple) {
            debug!("fetching asset {}", asset.name);

            let response = octocrab._get(asset.browser_download_url.as_str()).await?;

            let body_stream = BodyStream::new(response.into_body());

            let stream = BodyStream::new(body_stream)
                .map(|result| match result {
                    Ok(frame) => match frame.into_data() {
                        Ok(data) => Ok(data),
                        Err(_) => Err(ErrorKind::EmptyFrame.into()),
                    },
                    Err(error) => Err(error.into()),
                })
                .map_err(|error: Error| IoError::new(IoErrorKind::Other, error));

            let reader = stream.into_async_read(); // stream -> reader
            let mut decoder = GzipDecoder::new(reader); // reader -> decoder

            let mut buffer = Vec::new();
            decoder.read_to_end(&mut buffer).await?; // decode response into buffer
            debug!("decompressed archive to size {}", buffer.len());

            let mut archive = Archive::new(buffer.as_slice()); // create archive from buffer
            let mut file = archive.entries()?.next().ok_or(ErrorKind::FileNotFound)??; // get the file

            let mut content = Vec::new();
            file.read_to_end(&mut content)?; // read the file into a buffer
            debug!("got file with size {}", content.len());

            // install the binary to the destination
            transfer_binary(&client, &content, &destination, &os).await?;

            // return to break loop
            return Ok(());
        }
    }

    Err(ErrorKind::NoSuitableRelease.into())
}

async fn identify_os(client: &Client) -> Result<Os> {
    let result = client.execute("uname -a || ver").await?;

    if result.stdout.contains("Microsoft Windows") {
        Ok(Os::Windows)
    } else if result.stdout.contains("Linux")
        || result.stdout.contains("Darwin")
        || result.stdout.contains("BSD")
    {
        Ok(Os::UnixLike)
    } else {
        Err(ErrorKind::UnknownOs((result.stdout, result.stderr)).into())
    }
}

async fn get_triple(client: &Client, os: &Os) -> Result<String> {
    let command = match os {
        Os::Windows => WINDOWS_TARGET,
        Os::UnixLike => UNIX_TARGET,
    };

    let result = client.execute(command).await?;

    if result.exit_status == 0 {
        Ok(result.stdout.replace(['\n', '\r'], ""))
    } else {
        error!("target command failed {}", result.stderr);
        Err(ErrorKind::CommandFailed(result.exit_status).into())
    }
}

async fn transfer_binary(
    client: &Client,
    content: &[u8],
    destination: &Path,
    os: &Os,
) -> Result<()> {
    let channel = client.get_channel().await?; // get a channel
    channel.request_subsystem(true, "sftp").await?; // start sftp
    let sftp = SftpSession::new(channel.into_stream()).await?; // create sftp session
    debug!("created sftp session");

    // create the destination file
    let path = format_path(destination, os);
    let mut file = sftp.create(&path).await?;
    file.write_all(content).await?; // write the file to the destination
    debug!("wrote file to {}", path);

    Ok(())
}

async fn is_dir(client: &Client, os: &Os, path: &Path) -> Result<bool> {
    let parent_directory = path.parent().map_or(".".into(), |p| p.to_string_lossy());
    let path = format_path(path, os);

    let command = match os {
        Os::Windows => format!(
            "if exist \"{}\" ( if exist \"{}\" ( echo true ) else ( if exist \"{}\" ( echo false ) else ( echo not_found ))) else (echo dnf)",
            parent_directory, path, path
        ),
        Os::UnixLike => format!(
            "[ -d \"{}\" ] && ( [ -d \"{}\" ] && echo true || ([ -f \"{}\" ] && echo false || echo not_found )) || echo dnf",
            parent_directory, path, path
        ),
    };

    let result = client.execute(&command).await?;

    if result.exit_status == 0 {
        match result.stdout.chars().next() {
            Some('t') => Ok(true),
            Some('f') => Ok(false),
            Some('n') => Err(ErrorKind::FileNotFound.into()),
            Some('d') => Err(ErrorKind::DirectoryNotFound.into()),
            _ => Err(ErrorKind::CommandFailed(result.exit_status).into()),
        }
    } else {
        Err(ErrorKind::CommandFailed(result.exit_status).into())
    }
}

fn format_path(path: &Path, os: &Os) -> String {
    match os {
        Os::Windows => path.to_string_lossy().replace('/', "\\"),
        Os::UnixLike => path.to_string_lossy().replace('\\', "/"),
    }
}
