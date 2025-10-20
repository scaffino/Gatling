#!/usr/bin/env python3
"""
Script to analyze transaction finalization latency from validator.log

This script reads the validator.log file and calculates the latency for each
finalized transaction by comparing the log timestamp with the transaction's
embedded timestamp.
"""

import re
from datetime import datetime
import sys
import calendar


def parse_log_line(line):
    """
    Parse a log line to extract timestamp and transaction finalization info.
    
    Returns:
        tuple: (log_datetime, transaction_hash, tx_timestamp_ms) or None if not a finalization line
    """
    # Pattern to match finalization lines
    # Example: "Transaction a81454ee...947c8e (timestamp: 1760623008123 ms) is now final in block"
    finalization_pattern = r'Transaction ([a-f0-9]+) \(timestamp: (\d+) ms\) is now final in block'
    
    # Pattern to extract ISO-8601/RFC-3339 timestamp at the beginning
    # Example: "2025-10-16T13:56:48.643562Z"
    timestamp_pattern = r'^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)'
    
    timestamp_match = re.search(timestamp_pattern, line)
    finalization_match = re.search(finalization_pattern, line)
    
    if timestamp_match and finalization_match:
        log_timestamp_str = timestamp_match.group(1)
        transaction_hash = finalization_match.group(1)
        tx_timestamp_ms = int(finalization_match.group(2))
        
        # Parse ISO-8601 timestamp to datetime object (compatible with Python 3.5+)
        # Remove 'Z' and parse manually
        log_timestamp_str = log_timestamp_str.rstrip('Z')
        log_datetime = datetime.strptime(log_timestamp_str, '%Y-%m-%dT%H:%M:%S.%f')
        
        return (log_datetime, transaction_hash, tx_timestamp_ms)
    
    return None


def main():
    log_file_path = 'validator_gossip_all_10instances.log'
    
    try:
        with open(log_file_path, 'r') as f:
            lines = f.readlines()
    except IOError:
        print("Error: {0} not found".format(log_file_path))
        sys.exit(1)
    
    finalization_count = 0
    seen_transactions = set()  # Track transaction hashes we've already processed
    duplicate_count = 0  # Track how many duplicates we skipped
    latencies_ms = []  # Track all latencies for average calculation
    
    print("Transaction Finalization Latency Analysis")
    print("=" * 95)
    print("{:<66} {:<15} {:<15}".format('Transaction Hash', 'Latency (ms)', 'Latency (s)'))
    print("-" * 95)
    
    for line in lines:
        result = parse_log_line(line)
        if result:
            log_datetime, tx_hash, tx_timestamp_ms = result
            
            # Skip if we've already seen this transaction hash
            if tx_hash in seen_transactions:
                duplicate_count += 1
                continue
            
            # Mark this transaction as seen
            seen_transactions.add(tx_hash)
            
            # Convert log datetime to Unix timestamp in milliseconds
            # Python 3.5 compatible: use calendar.timegm()
            log_timestamp_ms = calendar.timegm(log_datetime.timetuple()) * 1000 + log_datetime.microsecond / 1000.0
            
            # Calculate latency: log_timestamp_ms - tx_timestamp_ms
            latency_ms = log_timestamp_ms - tx_timestamp_ms
            latency_s = latency_ms / 1000.0
            
            # Store latency for average calculation
            latencies_ms.append(latency_ms)
            
            # Output the result
            print("{:<66} {:<15.3f} {:<15.2f}".format(tx_hash, latency_ms, latency_s))
            finalization_count += 1
    
    print("-" * 95)
    print("Total finalized transactions found: {0}".format(finalization_count))
    if duplicate_count > 0:
        print("Duplicate finalizations skipped: {0}".format(duplicate_count))
    
    # Calculate and display average latency
    if finalization_count > 0:
        avg_latency_ms = sum(latencies_ms) / len(latencies_ms)
        avg_latency_s = avg_latency_ms / 1000.0
        print("Average latency: {0:.3f} ms ({1:.2f} s)".format(avg_latency_ms, avg_latency_s))
    
    if finalization_count == 0:
        print("\nNote: No finalized transactions found in the log file.")
        print("The script looks for lines matching the pattern:")
        print('  "Transaction <hash> (timestamp: <unix_timestamp_ms> ms) is now final in block..."')


if __name__ == "__main__":
    main()

