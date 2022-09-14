//
// Created by Ziyang Hu on 2022/7/3.
//

#include <iostream>
#include <memory>
#include "db.h"
#include "cozorocks/src/bridge/mod.rs.h"

Options default_db_options() {
    Options options = Options();
    options.bottommost_compression = kZSTD;
    options.compression = kLZ4Compression;
    options.level_compaction_dynamic_level_bytes = true;
    options.max_background_compactions = 4;
    options.max_background_flushes = 2;
    options.bytes_per_sync = 1048576;
    options.compaction_pri = kMinOverlappingRatio;
    BlockBasedTableOptions table_options;
    table_options.block_size = 16 * 1024;
    table_options.cache_index_and_filter_blocks = true;
    table_options.pin_l0_filter_and_index_blocks_in_cache = true;
    table_options.format_version = 5;

    auto table_factory = NewBlockBasedTableFactory(table_options);
    options.table_factory.reset(table_factory);

    return options;
}

ColumnFamilyOptions default_cf_options() {
    ColumnFamilyOptions options = ColumnFamilyOptions();
    options.bottommost_compression = kZSTD;
    options.compression = kLZ4Compression;
    options.level_compaction_dynamic_level_bytes = true;
    options.compaction_pri = kMinOverlappingRatio;
    BlockBasedTableOptions table_options;
    table_options.block_size = 16 * 1024;
    table_options.cache_index_and_filter_blocks = true;
    table_options.pin_l0_filter_and_index_blocks_in_cache = true;
    table_options.format_version = 5;

    auto table_factory = NewBlockBasedTableFactory(table_options);
    options.table_factory.reset(table_factory);

    return options;
}

shared_ptr<RocksDbBridge> open_db(const DbOpts &opts, RocksDbStatus &status, bool use_cmp,
                                  RustComparatorFn pri_cmp_impl,
                                  RustComparatorFn snd_cmp_impl) {
    auto options = default_db_options();
    auto cf_pri_opts = default_cf_options();
    auto cf_snd_opts = default_cf_options();

    if (opts.prepare_for_bulk_load) {
        options.PrepareForBulkLoad();
    }
    if (opts.increase_parallelism > 0) {
        options.IncreaseParallelism(opts.increase_parallelism);
    }
    if (opts.optimize_level_style_compaction) {
        options.OptimizeLevelStyleCompaction();
        cf_pri_opts.OptimizeLevelStyleCompaction();
        cf_snd_opts.OptimizeLevelStyleCompaction();
    }
    options.create_if_missing = opts.create_if_missing;
    options.paranoid_checks = opts.paranoid_checks;
    if (opts.enable_blob_files) {
        options.enable_blob_files = true;
        cf_pri_opts.enable_blob_files = true;
        cf_snd_opts.enable_blob_files = true;

        options.min_blob_size = opts.min_blob_size;
        cf_pri_opts.min_blob_size = opts.min_blob_size;
        cf_snd_opts.min_blob_size = opts.min_blob_size;

        options.blob_file_size = opts.blob_file_size;
        cf_pri_opts.blob_file_size = opts.blob_file_size;
        cf_snd_opts.blob_file_size = opts.blob_file_size;

        options.enable_blob_garbage_collection = opts.enable_blob_garbage_collection;
        cf_pri_opts.enable_blob_garbage_collection = opts.enable_blob_garbage_collection;
        cf_snd_opts.enable_blob_garbage_collection = opts.enable_blob_garbage_collection;
    }
    if (opts.use_bloom_filter) {
        BlockBasedTableOptions table_options;
        table_options.filter_policy.reset(NewBloomFilterPolicy(opts.bloom_filter_bits_per_key, false));
        table_options.whole_key_filtering = opts.bloom_filter_whole_key_filtering;
        cf_snd_opts.table_factory.reset(NewBlockBasedTableFactory(table_options));
        cf_pri_opts.table_factory.reset(NewBlockBasedTableFactory(table_options));
        options.table_factory.reset(NewBlockBasedTableFactory(table_options));
    }
    if (opts.pri_use_capped_prefix_extractor) {
        cf_pri_opts.prefix_extractor.reset(NewCappedPrefixTransform(opts.pri_capped_prefix_extractor_len));
    }
    if (opts.snd_use_capped_prefix_extractor) {
        cf_snd_opts.prefix_extractor.reset(NewCappedPrefixTransform(opts.snd_capped_prefix_extractor_len));
    }
    if (opts.pri_use_fixed_prefix_extractor) {
        cf_pri_opts.prefix_extractor.reset(NewFixedPrefixTransform(opts.pri_fixed_prefix_extractor_len));
    }
    if (opts.snd_use_fixed_prefix_extractor) {
        cf_pri_opts.prefix_extractor.reset(NewFixedPrefixTransform(opts.snd_fixed_prefix_extractor_len));
    }
    RustComparator *pri_cmp = nullptr;
    RustComparator *snd_cmp = nullptr;
    if (use_cmp) {
        pri_cmp = new RustComparator(
                string(opts.pri_comparator_name),
                opts.pri_comparator_different_bytes_can_be_equal,
                pri_cmp_impl);
        cf_pri_opts.comparator = pri_cmp;

        snd_cmp = new RustComparator(
                string(opts.snd_comparator_name),
                opts.snd_comparator_different_bytes_can_be_equal,
                snd_cmp_impl);
        cf_snd_opts.comparator = snd_cmp;
    }
    options.create_missing_column_families = true;

    shared_ptr<RocksDbBridge> db = make_shared<RocksDbBridge>();

    db->db_path = string(opts.db_path);
    db->pri_comparator.reset(pri_cmp);
    db->snd_comparator.reset(snd_cmp);

    std::vector<ColumnFamilyDescriptor> column_families;
    column_families.emplace_back(ColumnFamilyDescriptor(
            rocksdb::kDefaultColumnFamilyName, cf_pri_opts));
    column_families.emplace_back(ColumnFamilyDescriptor(
            "relation", cf_snd_opts));

    TransactionDB *txn_db = nullptr;
    write_status(
            TransactionDB::Open(options, TransactionDBOptions(), db->db_path, column_families, &db->cf_handles,
                                &txn_db),
            status);
    db->db.reset(txn_db);
    db->destroy_on_exit = opts.destroy_on_exit;


    return db;
}

RocksDbBridge::~RocksDbBridge() {
    if (destroy_on_exit && (db != nullptr)) {
        cerr << "destroying database on exit: " << db_path << endl;
        auto status = db->Close();
        if (!status.ok()) {
            cerr << status.ToString() << endl;
        }
        db.reset();
        Options options{};
        auto status2 = DestroyDB(db_path, options);
        if (!status2.ok()) {
            cerr << status2.ToString() << endl;
        }
    }
}
