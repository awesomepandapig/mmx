#!/bin/sh
# Builds the images for a local cluster with Apple `container`, one at a time.
#
#   deploy/scripts/local-images.sh                  everything values-local.yaml needs
#   deploy/scripts/local-images.sh api market-data       only these images
#
# `container` has no Bake, so this is the loop Bake would run. It builds from a copy of the tracked
# and new files outside /Users, because the builder is very slow when a large directory such as
# target/ sits under the context, even if it's excluded.
set -eu

cd "$(dirname "$0")/../.."
tag=${TAG:-dev}
context=${CONTEXT:-/tmp/mmx-image-context}

rm -rf "$context"
mkdir -p "$context"
git ls-files -z --cached --others --exclude-standard | rsync -a --from0 --files-from=- ./ "$context/"

build() {
    image=$1
    shift
    echo "building mmx/$image:$tag"
    container build --file "$context/deploy/docker/Dockerfile" --tag "mmx/$image:$tag" "$@" "$context"
}

if [ "$#" -eq 0 ]; then
    set -- node matching-engine api websocket market-data trade-reporter web
fi

for image in "$@"; do
    case "$image" in
        node | matching-engine | tools)
            build "$image" --target "$image"
            ;;
        web | docs)
            build "$image" --target remix --build-arg "APP=$image"
            ;;
        auth)
            build auth --target auth --build-arg APP=auth
            ;;
        observer)
            build observer --target service --build-arg SERVICE=sbx-observer
            ;;
        *)
            build "$image" --target service --build-arg "SERVICE=$image"
            ;;
    esac
done
