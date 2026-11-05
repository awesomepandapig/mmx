// Every image. From the repository root:
//
//   docker buildx bake -f deploy/docker/docker-bake.hcl                  all images, both architectures
//   PLATFORMS=linux/amd64 docker buildx bake -f deploy/docker/docker-bake.hcl core --load
//
// To add a Rust service, add it to the `service` matrix and the `core` group. `outbox` only needs its
// line once it's in the workspace, because every image in the matrix must exist to build.

variable "REGISTRY" {
  default = "ghcr.io/awesomepandapig/mmx"
}

variable "TAG" {
  default = "dev"
}

variable "PLATFORMS" {
  default = "linux/amd64,linux/arm64"
}

// 0 for published images. See build-release in the Dockerfile.
variable "CARGO_INCREMENTAL" {
  default = "1"
}

// Set by CI to share the dependency layers through the GitHub Actions cache. Only the `deps` group
// writes it, because one copy per image wouldn't fit in the cache's size limit.
variable "CACHE_SCOPE" {
  default = ""
}

// Unset VITE_ values are dropped, so the app uses its own defaults. An empty string wouldn't be.
variable "VITE_DOMAIN" {
  default = null
}

variable "VITE_API_URL" {
  default = null
}

variable "VITE_AUTH_URL" {
  default = null
}

variable "VITE_WS_URL" {
  default = null
}

variable "VITE_DOCS_URL" {
  default = null
}

group "default" {
  targets = ["core", "observer", "tools", "web", "docs", "auth"]
}

group "core" {
  targets = ["node", "matching-engine", "api", "websocket", "level2", "ticker", "candles", "trade-reporter"]
}

target "_common" {
  context    = "."
  dockerfile = "deploy/docker/Dockerfile"
  platforms  = split(",", PLATFORMS)
  args = {
    CARGO_INCREMENTAL = CARGO_INCREMENTAL
  }
  cache-from = compact([
    CACHE_SCOPE == "" ? "" : "type=gha,scope=${CACHE_SCOPE}-deps-release",
    CACHE_SCOPE == "" ? "" : "type=gha,scope=${CACHE_SCOPE}-deps-engine",
  ])
}

group "deps" {
  targets = ["deps-release", "deps-engine"]
}

target "deps-stage" {
  name     = stage
  inherits = ["_common"]
  matrix = {
    stage = ["deps-release", "deps-engine"]
  }
  target   = stage
  output   = ["type=cacheonly"]
  cache-to = compact([CACHE_SCOPE == "" ? "" : "type=gha,scope=${CACHE_SCOPE}-${stage},mode=max"])
}

target "service" {
  name     = item.image
  inherits = ["_common"]
  matrix = {
    item = [
      { image = "api", bin = "api" },
      { image = "websocket", bin = "websocket" },
      { image = "level2", bin = "level2" },
      { image = "ticker", bin = "ticker" },
      { image = "candles", bin = "candles" },
      { image = "trade-reporter", bin = "trade-reporter" },
      { image = "outbox", bin = "outbox" },
      { image = "observer", bin = "sbx-observer" },
    ]
  }
  target = "service"
  args = {
    SERVICE = item.bin
  }
  tags = ["${REGISTRY}/${item.image}:${TAG}"]
}

target "matching-engine" {
  inherits = ["_common"]
  target   = "matching-engine"
  tags     = ["${REGISTRY}/matching-engine:${TAG}"]
}

target "node" {
  inherits = ["_common"]
  target   = "node"
  tags     = ["${REGISTRY}/node:${TAG}"]
}

target "tools" {
  inherits = ["_common"]
  target   = "tools"
  tags     = ["${REGISTRY}/tools:${TAG}"]
}

target "test" {
  inherits = ["_common"]
  target   = "test"
  output   = ["type=cacheonly"]
}

target "web" {
  inherits = ["_common"]
  target   = "remix"
  args = {
    APP           = "web"
    VITE_DOMAIN   = VITE_DOMAIN
    VITE_API_URL  = VITE_API_URL
    VITE_AUTH_URL = VITE_AUTH_URL
    VITE_WS_URL   = VITE_WS_URL
    VITE_DOCS_URL = VITE_DOCS_URL
  }
  tags = ["${REGISTRY}/web:${TAG}"]
}

target "docs" {
  inherits = ["_common"]
  target   = "remix"
  args = {
    APP          = "docs"
    VITE_DOMAIN  = VITE_DOMAIN
    VITE_API_URL = VITE_API_URL
    VITE_WS_URL  = VITE_WS_URL
  }
  tags = ["${REGISTRY}/docs:${TAG}"]
}

target "auth" {
  inherits = ["_common"]
  target   = "auth"
  args = {
    APP = "auth"
  }
  tags = ["${REGISTRY}/auth:${TAG}"]
}
