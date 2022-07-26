#!/bin/bash

# Based of https://github.com/mandreyel/cratetorrent/blob/34aa13835872a14f00d4a334483afff79181999f/tests/test_single_connection_download.sh

# Set up a single transmission seeder and a rust torrent leecher
# and asserts that rust downloads a single 1 MiB file from the seed
# correctly.

# Returns the container's IP address in the local Docker network.
#
# Arguments:
# - $1 container name
function get_container_ip {
    cont=$1
    docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${cont}"
}

# where the torrent file and metainfo are saved
assets_dir="$(pwd)/assets"

# name of docker container
seed_container="tr-seed"

if [ ! -d "${assets_dir}" ]; then
    echo "Creating assets directory ${assets_dir}"
    mkdir "${assets_dir}"
fi

tr_seed_dir="${assets_dir}/${seed_container}"
if [ ! -d "${tr_seed_dir}" ]; then
    echo "Creating seed ${seed_container} directory at ${tr_seed_dir}"
    mkdir "${tr_seed_dir}"
fi

# start the detached transmission docker container
docker compose up --detach --remove-orphans --wait

# check ip of seeder
seed_ip="$(get_container_ip "${seed_container}")"
echo "Seed available on local Docker net at IP: ${seed_ip}"

# create a random 1MiB file inside the container
head -c 1M < /dev/urandom > "assets/${seed_container}/downloads/complete/testfile"

# create .torrent metainfo file
docker exec -u 1000 tr-seed transmission-create -o "/watch/testfile.torrent" "downloads/complete/testfile"

sleep 10

# download torrent with rust client
# cargo build
./target/debug/test-cli --torrentfile "assets/${seed_container}/watch/testfile.torrent.added" --seeder "${seed_ip}:51413"

# verify torrent
echo -e "\e[92mSUCCESS\e[0m: downloaded file matches source file"

# docker compose down