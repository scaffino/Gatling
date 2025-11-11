# Create a single destination folder for all logs
mkdir -p logs/validator

# Download logs from each VM and insert VM identifier after "val_" to avoid filename conflicts
for file in $(ssh root@gatling-nyc "ls /root/alto/logs/validator/val_*.log 2>/dev/null"); do
    filename=$(basename "$file")
    newname=$(echo "$filename" | sed 's/^val_/val_nyc_/')
    scp "root@gatling-nyc:$file" "logs/validator/$newname"
done

for file in $(ssh root@gatling-india "ls /root/alto/logs/validator/val_*.log 2>/dev/null"); do
    filename=$(basename "$file")
    newname=$(echo "$filename" | sed 's/^val_/val_india_/')
    scp "root@gatling-india:$file" "logs/validator/$newname"
done

for file in $(ssh root@gatling-london "ls /root/alto/logs/validator/val_*.log 2>/dev/null"); do
    filename=$(basename "$file")
    newname=$(echo "$filename" | sed 's/^val_/val_london_/')
    scp "root@gatling-london:$file" "logs/validator/$newname"
done

for file in $(ssh root@gatling-syd "ls /root/alto/logs/validator/val_*.log 2>/dev/null"); do
    filename=$(basename "$file")
    newname=$(echo "$filename" | sed 's/^val_/val_syd_/')
    scp "root@gatling-syd:$file" "logs/validator/$newname"
done

for file in $(ssh root@gatling-singapore "ls /root/alto/logs/validator/val_*.log 2>/dev/null"); do
    filename=$(basename "$file")
    newname=$(echo "$filename" | sed 's/^val_/val_singapore_/')
    scp "root@gatling-singapore:$file" "logs/validator/$newname"
done

for file in $(ssh root@gatling-frank "ls /root/alto/logs/validator/val_*.log 2>/dev/null"); do
    filename=$(basename "$file")
    newname=$(echo "$filename" | sed 's/^val_/val_frank_/')
    scp "root@gatling-frank:$file" "logs/validator/$newname"
done

for file in $(ssh root@gatling-sf "ls /root/alto/logs/validator/val_*.log 2>/dev/null"); do
    filename=$(basename "$file")
    newname=$(echo "$filename" | sed 's/^val_/val_sf_/')
    scp "root@gatling-sf:$file" "logs/validator/$newname"
done