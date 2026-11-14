# DNA-DB CMS edition — MySQL-compatible server image.
#
# The `mysqld` mode speaks the MySQL wire protocol on :3306 (WordPress,
# WooCommerce, Joomla, Drupal, wp-cli, mysqldump — anything that talks
# MySQL connects unchanged) and serves the embedded web panel +
# Prometheus /metrics on :9310.
#
#   Build:  docker build -t dna-db-core:cms .
#   Run:    docker run -d --name dna-db -p 3306:3306 -p 9310:9310 \
#             -v dna_data:/var/lib/dna-db dna-db-core:cms
#
# The web panel (databases, dual spirals, storage roots, /metrics) is on
# http://localhost:9310 — login with the wire credentials.

FROM rust:1-slim-bookworm AS build
WORKDIR /src

# Dependency manifest first for layer caching (dummy build, then real one).
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
RUN mkdir -p src && \
    echo "pub fn placeholder() {}" > src/lib.rs && \
    cargo build --release --features cms -q 2>/dev/null || true && \
    rm -f src/lib.rs target/release/dna-db-core 2>/dev/null || true

# Real sources (patch.crates-io points at vendor/reed-solomon-erasure).
COPY src ./src
RUN cargo build --release --features cms && \
    strip target/release/dna-db-core

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -m dnadb

COPY --from=build /src/target/release/dna-db-core /usr/local/bin/dna-db-core

# Data directory (engine + spiral catalog + per-database Strand A/B pairs).
VOLUME /var/lib/dna-db
EXPOSE 3306 9310

USER dnadb
WORKDIR /var/lib/dna-db

# Change --user to your credentials (repeatable). Without users the wire
# listener runs in trust mode (any login) and says so loudly in the logs.
ENTRYPOINT ["/usr/local/bin/dna-db-core"]
CMD ["mysqld", \
     "--data-dir", "/var/lib/dna-db", \
     "--bind", "0.0.0.0", "--port", "3306", \
     "--user", "root:dna-db", \
     "--admin-port", "9310", "--admin-bind", "0.0.0.0", \
     "--sync", "strict"]

# Wire-port probe; the binary itself has no HTTP health endpoint on 3306.
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
  CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/3306 || exit 1'
