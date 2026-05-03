# syntax=docker/dockerfile:1.7

ARG UBUNTU_VERSION=26.04
FROM ubuntu:${UBUNTU_VERSION}

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && update-ca-certificates \
    && groupadd --gid 714 docker \
    && useradd --uid 714 --gid docker --no-create-home --shell /usr/sbin/nologin docker \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sL https://github.com/foundry-rs/foundry/releases/download/v1.7.0/foundry_v1.7.0_linux_amd64.tar.gz | tar -xz \
  && mv forge cast anvil chisel /usr/local/bin/

COPY --chmod=0755 build-artifacts/arkiv-node /usr/local/bin/arkiv-node
COPY --chmod=0755 build-artifacts/arkiv-cli /usr/local/bin/arkiv-cli
COPY --chmod=0755 build-artifacts/arkiv-storaged /usr/local/bin/arkiv-storaged

RUN mkdir -p /app \
  && chown -R docker:docker /app
WORKDIR /app

USER docker

ENTRYPOINT ["arkiv-node"]
