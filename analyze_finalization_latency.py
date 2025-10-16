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
        tuple: (log_datetime, transaction_hash, tx_timestamp) or None if not a finalization line
    """
    # Pattern to match finalization lines
    # Example: "Transaction a81454ee...947c8e (timestamp: 1760623008) is now final in block"
    finalization_pattern = r'Transaction ([a-f0-9]+) \(timestamp: (\d+)\) is now final in block'
    
    # Pattern to extract ISO-8601/RFC-3339 timestamp at the beginning
    # Example: "2025-10-16T13:56:48.643562Z"
    timestamp_pattern = r'^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)'
    
    timestamp_match = re.search(timestamp_pattern, line)
    finalization_match = re.search(finalization_pattern, line)
    
    if timestamp_match and finalization_match:
        log_timestamp_str = timestamp_match.group(1)
        transaction_hash = finalization_match.group(1)
        tx_timestamp = int(finalization_match.group(2))
        
        # Parse ISO-8601 timestamp to datetime object (compatible with Python 3.5+)
        # Remove 'Z' and parse manually
        log_timestamp_str = log_timestamp_str.rstrip('Z')
        log_datetime = datetime.strptime(log_timestamp_str, '%Y-%m-%dT%H:%M:%S.%f')
        
        return (log_datetime, transaction_hash, tx_timestamp)
    
    return None


def main():
    log_file_path = 'validator.log'
    
    try:
        with open(log_file_path, 'r') as f:
            lines = f.readlines()
    except IOError:
        print("Error: {0} not found".format(log_file_path))
        sys.exit(1)
    
    finalization_count = 0
    
    print("Transaction Finalization Latency Analysis")
    print("=" * 80)
    print("{:<66} {:<15}".format('Transaction Hash', 'Latency (seconds)'))
    print("-" * 80)
    
    for line in lines:
        result = parse_log_line(line)
        if result:
            log_datetime, tx_hash, tx_timestamp = result
            
            # Convert log datetime to Unix timestamp (seconds since epoch)
            # Python 3.5 compatible: use calendar.timegm()
            log_timestamp = calendar.timegm(log_datetime.timetuple()) + log_datetime.microsecond / 1000000.0
            
            # Calculate latency: log_timestamp - tx_timestamp
            latency = log_timestamp - tx_timestamp
            
            # Output the result
            print("{:<66} {:<15.6f}".format(tx_hash, latency))
            finalization_count += 1
    
    print("-" * 80)
    print("Total finalized transactions found: {0}".format(finalization_count))
    
    if finalization_count == 0:
        print("\nNote: No finalized transactions found in the log file.")
        print("The script looks for lines matching the pattern:")
        print('  "Transaction <hash> (timestamp: <unix_timestamp>) is now final in block..."')


if __name__ == "__main__":
    main()

