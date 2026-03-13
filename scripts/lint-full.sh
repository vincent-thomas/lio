#!/usr/bin/env bash

set -e

./scripts/lint.sh

cargo hack check --feature-powerset --lib --tests -p lio
