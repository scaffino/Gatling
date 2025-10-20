#!/bin/bash

# Number of transactions to submit (default: 100)
NUM_TXS=${1:-100}

echo "Submitting $NUM_TXS transactions..."

for i in $(seq 1 $NUM_TXS); do
    echo "Submitting transaction $i/$NUM_TXS (amount: $i)"
    ./target/release/submit_tx --validator-all --sender-seed 999 --receiver 3ecf551aeb957616c6c8aa603634ea55845f88712a58745e58a71fe988bb967a --amount $i
done

echo "Done! Submitted $NUM_TXS transactions."