#!/bin/bash

KEY_PATH=${KEY_PATH:-/opt/app/storage}

# Set up key storage
mkdir -p $KEY_PATH

#Set up all needed secrets
STORE_NAME=$1
SEED_PHRASE=$2
KEY_PASS=$3
PLATFORM_KEY=$4

# Set the key
# Depending on the machine it's running(even inside the docker container) sometimes the external '' are needed
# other times it's not needed, so set `ADD_QUOTES` in the docker compose if your machine needs it(This way we don't need to rebuid the docker image for each machine)
if [ -z ${ADD_QUOTES+x} ]; then
    printf "%s %s %s %s %s %s %s %s %s %s %s %s" $SEED_PHRASE > /opt/app/storage/$STORE_NAME
else
    printf '"%s %s %s %s %s %s %s %s %s %s %s %s"' $SEED_PHRASE > /opt/app/storage/$STORE_NAME
fi

# Set key pass (When read by the wallet it will be unset as part of its start up)
export KEY_PASS
export PLATFORM_KEY

# Run wallet
wallet
