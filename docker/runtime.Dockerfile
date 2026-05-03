# syntax=docker/dockerfile:1.7

ARG UBUNTU_VERSION=26.04
FROM ubuntu:${UBUNTU_VERSION}

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && update-ca-certificates \
    && groupadd --system docker \
    && useradd --system --gid docker --create-home --home-dir /home/docker --shell /usr/sbin/nologin docker \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sL https://github.com/foundry-rs/foundry/releases/download/v1.7.0/foundry_v1.7.0_linux_amd64.tar.gz | tar -xz \
  && mv forge cast anvil chisel /usr/local/bin/

COPY --chmod=0755 build-artifacts/arkiv-node /usr/local/bin/arkiv-node
COPY --chmod=0755 build-artifacts/arkiv-cli /usr/local/bin/arkiv-cli
COPY --chmod=0755 build-artifacts/arkiv-storaged /usr/local/bin/arkiv-storaged

USER dockerr
WORKDIR /home/docker

ENTRYPOINT ["arkiv-node"]
