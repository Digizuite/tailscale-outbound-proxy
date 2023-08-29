FROM rust:1.72 as build
WORKDIR /usr/src

# Install musl-gcc
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools

# Download the target for static linking.
RUN rustup target add x86_64-unknown-linux-musl

# Create a dummy project and build the app's dependencies.
# If the Cargo.toml and Cargo.lock files have not changed,
# we can use the docker build cache and skip this slow step.
RUN mkdir -p ./src/ && echo 'fn main(){println!("NOT COMPILED CORRECTLY");}' > ./src/main.rs
COPY Cargo.lock Cargo.toml ./
RUN cargo build --target x86_64-unknown-linux-musl --release

# Copy the source and build the application.
COPY . ./
RUN touch ./src/main.rs
RUN cargo build --locked --frozen --offline --target x86_64-unknown-linux-musl --release

# Copy the statically-linked binary into a scratch container.
FROM scratch
COPY --from=build /usr/src/target/x86_64-unknown-linux-musl/release/tailscale-outbound-proxy .
USER 1000
ENTRYPOINT ["./tailscale-outbound-proxy"]
