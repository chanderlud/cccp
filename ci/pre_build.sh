#!/bin/bash

# Define the URL and file name
URL="https://github.com/protocolbuffers/protobuf/releases/download/v25.2/protoc-25.2-linux-x86_64.zip"
ZIP_FILE="protoc-25.2-linux-x86_64.zip"
BIN_FOLDER="bin"

apt-get install -y wget

# Download the ZIP file
wget $URL

# Extract the ZIP file
unzip $ZIP_FILE

# Move a file from the bin folder to /bin
mv $BIN_FOLDER/protoc /bin/
