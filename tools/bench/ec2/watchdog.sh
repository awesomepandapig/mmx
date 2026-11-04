#!/bin/sh
# A second, independent shutdown: terminates every bench instance from this Mac after N minutes.
# Usage: watchdog.sh MINUTES
minutes=${1:?minutes required}
sleep $((minutes * 60))
ids=$(aws ec2 describe-instances --region us-east-1 \
  --filters Name=tag:Project,Values=mmx-cluster-bench Name=instance-state-name,Values=pending,running \
  --query 'Reservations[].Instances[].InstanceId' --output text)
[ -n "$ids" ] && aws ec2 terminate-instances --region us-east-1 --instance-ids $ids
