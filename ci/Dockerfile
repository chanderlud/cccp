ARG CROSS_BASE_IMAGE
FROM $CROSS_BASE_IMAGE

RUN apt-get -y update && \
apt-get install -y wget unzip && \
wget https://github.com/protocolbuffers/protobuf/releases/download/v25.2/protoc-25.2-linux-x86_64.zip && \
unzip protoc-25.2-linux-x86_64.zip && \
mv bin/protoc /usr/local/bin/