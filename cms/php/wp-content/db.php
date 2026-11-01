<?php
/**
 * DNA-DB CMS Edition — WordPress drop-in (PROTOTYPE, phase 1).
 * SPDX-License-Identifier: Apache-2.0
 *
 * Place this file at wp-content/db.php. WordPress loads it *before*
 * instantiating wpdb, so defining a wpdb subclass here replaces the MySQL
 * connection entirely — the same mechanism the official SQLite integration
 * uses. No core file is touched.
 *
 * Required constants in wp-config.php (instead of the MySQL block):
 *
 *   define('DNA_DB_URL',  'http://127.0.0.1:8443');   // admin/panel port
 *   define('DNA_DB_USER', 'wp_app');                  // panel user (operator role)
 *   define('DNA_DB_PASS', '…');                       // its password
 *   // optional: define('DNA_DB_DBNAME', 'wordpress');
 *
 * The class speaks to the DNA-DB panel HTTP API:
 *   POST /api/login {user, pass} -> {token}   (cached for the PHP request)
 *   POST /api/query {query, kind:'sql'} -> {columns, rows, message, …}
 *
 * What works (matches the cms::compat gate on the Rust side):
 *   - SELECT / INSERT / UPDATE / DELETE on tables created via the gate;
 *   - AUTO_INCREMENT + $wpdb->insert_id (engine sequences);
 *   - SET NAMES / SET sql_mode / FOUND_ROWS() / LAST_INSERT_ID() shims;
 *   - SHOW TABLES [LIKE], SHOW FULL COLUMNS, SHOW INDEX (dbDelta path);
 *   - escape()/prepare() with MySQL string escaping.
 *
 * Known prototype limitations (docs/cms/wp_queries_corpus.txt is the spec):
 *   - no transaction API surface yet (WP core rarely needs it; phase 2);
 *   - rows are decoded from JSON, so BLOB columns come back as strings;
 *   - plugin DDL with composite UNIQUE keys degrades (first column only);
 *   - error strings are engine messages; errno mapping lives server-side.
 */

if (!defined('ABSPATH')) {
    exit;
}

// wpdb is not loaded yet when db.php is included — pull it in ourselves.
// WP 6.1+ ships class-wpdb.php; older branches use wp-db.php.
if (!class_exists('wpdb')) {
    if (file_exists(ABSPATH . WPINC . '/class-wpdb.php')) {
        require_once ABSPATH . WPINC . '/class-wpdb.php';
    } else {
        require_once ABSPATH . WPINC . '/wp-db.php';
    }
}

if (!class_exists('DNA_DB_wpdb')) {

    class DNA_DB_wpdb extends wpdb
    {
        /** @var string|null Panel API base URL, no trailing slash. */
        public $dna_url;
        /** @var string|null Panel user (operator role is enough). */
        public $dna_user;
        /** @var string|null Panel password. */
        public $dna_pass;
        /** @var array<string,mixed> Last raw API response fields. */
        public $dna_last;
        /** @var string|null Bearer token, per PHP request. */
        protected static $dna_token;

        /**
         * Replaces wpdb::__construct: no mysqli connection is opened, the
         * HTTP transport is lazy. Required wpdb fields are initialised by
         * hand (the SQLite drop-in uses the same technique).
         */
        public function __construct()
        {
            $this->dna_url  = untrailingslashit(defined('DNA_DB_URL') ? (string) DNA_DB_URL : 'http://127.0.0.1:8443');
            $this->dna_user = (string) (defined('DNA_DB_USER') ? DNA_DB_USER : '');
            $this->dna_pass = (string) (defined('DNA_DB_PASS') ? DNA_DB_PASS : '');

            $this->dbh           = null;
            $this->ready         = true;
            $this->prefix        = isset($GLOBALS['table_prefix']) ? (string) $GLOBALS['table_prefix'] : 'wp_';
            $this->base_prefix   = $this->prefix;
            $this->last_error    = '';
            $this->num_queries   = 0;
            $this->rows_affected = 0;
            $this->insert_id     = 0;
            $this->last_query    = '';
            $this->last_result   = array();
            $this->result        = null;
            $this->check_current_query = true;

            if (defined('DNA_DB_DBNAME')) {
                $this->dbname = (string) DNA_DB_DBNAME;
            }
            $this->set_prefix($this->prefix);
        }

        /* -----------------------------------------------------------------
         * Transport
         * --------------------------------------------------------------- */

        /**
         * POST helper: wp_remote_post when available (normal WP load order),
         * raw streams otherwise (defensive — db.php loads early).
         *
         * @param string $url  Full URL.
         * @param string $body Raw JSON body.
         * @return array{code:int, body:string}|null
         */
        protected function dna_post($url, $body)
        {
            if (function_exists('wp_remote_post')) {
                $res = wp_remote_post($url, array(
                    'timeout' => 30,
                    'headers' => array('Content-Type' => 'application/json'),
                    'body'    => $body,
                ));
                if (is_wp_error($res)) {
                    $this->last_error = 'DNA-DB request failed: ' . $res->get_error_message();
                    return null;
                }
                return array(
                    'code' => (int) wp_remote_retrieve_response_code($res),
                    'body' => (string) wp_remote_retrieve_body($res),
                );
            }
            $ctx = stream_context_create(array(
                'http' => array(
                    'method'        => 'POST',
                    'timeout'       => 30,
                    'header'        => "Content-Type: application/json\r\n",
                    'content'       => $body,
                    'ignore_errors' => true,
                ),
            ));
            $raw = @file_get_contents($url, false, $ctx);
            if (false === $raw) {
                $err = error_get_last();
                $this->last_error = 'DNA-DB request failed: '
                    . (isset($err['message']) ? $err['message'] : 'transport error');
                return null;
            }
            $code = 0;
            if (!empty($http_response_header)) {
                foreach ($http_response_header as $h) {
                    if (preg_match('#^HTTP/\S+\s+(\d{3})#', $h, $m)) {
                        $code = (int) $m[1];
                    }
                }
            }
            return array('code' => $code, 'body' => $raw);
        }

        /** Bearer token for the panel API (cached per PHP request). */
        protected function dna_token()
        {
            if (null !== self::$dna_token) {
                return self::$dna_token;
            }
            $res = $this->dna_post(
                $this->dna_url . '/api/login',
                (string) wp_json_encode(array('user' => $this->dna_user, 'pass' => $this->dna_pass))
            );
            if (null === $res) {
                $this->last_error = 'DNA-DB login failed: ' . $this->last_error;
                return '';
            }
            $json  = json_decode($res['body'], true);
            $token = (is_array($json) && isset($json['token'])) ? (string) $json['token'] : '';
            if ('' === $token) {
                $this->last_error = 'DNA-DB login failed: no token in response';
                return '';
            }
            self::$dna_token = $token;
            return $token;
        }

        /**
         * Sends one statement to the compat gate. Returns the decoded
         * response array or null on transport/API failure (last_error set).
         *
         * @param string $sql Raw SQL in the MySQL dialect.
         * @return array|null
         */
        protected function dna_query($sql)
        {
            $this->last_error = '';
            $token = $this->dna_token();
            if ('' === $token) {
                return null;
            }
            $res = $this->dna_post(
                $this->dna_url . '/api/query',
                (string) wp_json_encode(array('query' => $sql, 'kind' => 'sql'))
            );
            if (null === $res) {
                $this->last_error = 'DNA-DB request failed: ' . $this->last_error;
                return null;
            }
            $json = json_decode($res['body'], true);
            if ($res['code'] >= 400 || !is_array($json)) {
                $this->last_error = (is_array($json) && isset($json['error']))
                    ? (string) $json['error']
                    : ('DNA-DB HTTP ' . $res['code']);
                return null;
            }
            $this->dna_last = $json;
            return $json;
        }

        /* -----------------------------------------------------------------
         * wpdb surface
         * --------------------------------------------------------------- */

        /**
         * Runs one statement. Mirrors wpdb::query(): returns the number of
         * rows for SELECTs, the affected-rows count for DML, 0 for DDL,
         * false on error (last_error populated).
         *
         * @param string $query
         * @return int|false
         */
        public function query($query)
        {
            if (!isset($this->ready) || !$this->ready) {
                return false;
            }
            $query = (string) $query;
            if (false !== strpos($query, "\0")) {
                return false;
            }
            $this->flush();
            $this->last_query = $query;
            $this->num_queries++;

            $res = $this->dna_query($query);
            if (null === $res) {
                $this->last_result = array();
                return false;
            }

            $rows  = isset($res['rows']) && is_array($res['rows']) ? $res['rows'] : array();
            $message = isset($res['message']) ? (string) $res['message'] : '';
            $upper = strtoupper(ltrim($query));
            $is_read = (0 === strpos($upper, 'SELECT')) || (0 === strpos($upper, 'SHOW'))
                || (0 === strpos($upper, 'DESCRIBE')) || (0 === strpos($upper, 'EXPLAIN'));

            if ($is_read) {
                // wpdb expects an array of stdClass objects in last_result.
                $out = array();
                foreach ($rows as $row) {
                    $obj = new stdClass();
                    if (is_array($row)) {
                        foreach ($row as $k => $v) {
                            $obj->{$k} = is_scalar($v) ? $v : (null === $v ? null : (string) $v);
                        }
                    }
                    $out[] = $obj;
                }
                $this->last_result = $out;
                $this->num_rows    = count($out);
                return $this->num_rows;
            }

            // DML / DDL.
            $this->last_result   = array();
            $this->rows_affected = 0;
            if (preg_match('/(\d+)\s+row\(s\)/i', $message, $m)) {
                $this->rows_affected = (int) $m[1];
            }
            if (0 === strpos($upper, 'INSERT')) {
                $this->insert_id = 0;
                foreach ($rows as $row) {
                    if (isset($row['pk']) && is_numeric($row['pk'])) {
                        $this->insert_id = max($this->insert_id, (int) $row['pk']);
                    }
                }
            }
            $is_ddl = (0 === strpos($upper, 'CREATE')) || (0 === strpos($upper, 'ALTER'))
                || (0 === strpos($upper, 'DROP')) || (0 === strpos($upper, 'TRUNCATE'));
            return $is_ddl ? 0 : $this->rows_affected;
        }

        /**
         * MySQL-style string escaping (covers wpdb::escape/_real_escape).
         *
         * @param string|array $data
         * @return string|array
         */
        public function escape($data)
        {
            if (is_array($data)) {
                foreach ($data as $k => $v) {
                    $data[$k] = $this->escape($v);
                }
                return $data;
            }
            return $this->_real_escape((string) $data);
        }

        /**
         * @param string $string
         * @return string
         */
        public function _real_escape($string)
        {
            return addcslashes((string) $string, "\x00\\'\n\r\x1a");
        }

        /**
         * The HTTP transport is stateless — no reconnect dance.
         * @param bool $allow_bail Unused.
         * @return bool
         */
        public function db_connect($allow_bail = true)
        {
            unset($allow_bail);
            return true;
        }

        /**
         * Database selection is bookkeeping only (single-tenant gate).
         * @param string $db
         * @return bool
         */
        public function select($db)
        {
            $this->dbname = (string) $db;
            return true;
        }

        /**
         * wpdb checks the mysqli handle; the HTTP transport is stateless.
         * @param bool $select_db Unused.
         * @return bool
         */
        public function check_connection($select_db = true)
        {
            unset($select_db);
            return true;
        }

        /**
         * wpdb::db_version() queries the server; the compat gate answers
         * VERSION() with a MySQL 8.0-compatible tag either way.
         * @return string
         */
        public function db_version()
        {
            return '8.0.36';
        }

        /**
         * @param string $db_cap
         * @return bool
         */
        public function has_cap($db_cap)
        {
            switch (strtolower((string) $db_cap)) {
                case 'collation':
                case 'group_concat':
                case 'subqueries':
                case 'set_charset':
                    return true;
                default:
                    return parent::has_cap($db_cap);
            }
        }

        /**
         * wpdb::set_sql_mode sends SET sql_mode — the gate handles it, but
         * avoid the round trip and keep wpdb's bookkeeping simple.
         * @param array $modes
         * @return array
         */
        public function set_sql_mode($modes = array())
        {
            unset($modes);
            return array();
        }

        /**
         * Charset detection needs a live handle; the CMS edition always
         * speaks utf8mb4.
         * @param string $table
         * @return string|false
         */
        public function get_table_charset($table)
        {
            unset($table);
            return 'utf8mb4';
        }

        /**
         * @param string $table
         * @param string $column
         * @return string|false
         */
        public function get_col_charset($table, $column)
        {
            unset($table, $column);
            return 'utf8mb4';
        }
    }

    $GLOBALS['wpdb'] = new DNA_DB_wpdb();
}
