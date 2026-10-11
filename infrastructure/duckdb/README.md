# duckdb
Local research/ETL. Query MinIO Parquet directly:
`INSTALL httpfs; LOAD httpfs; SELECT * FROM read_parquet('s3://posteriors/mlb/*.parquet');`
