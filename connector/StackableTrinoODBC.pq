// Power Query custom connector for Trino via the Stackable ODBC driver.
//
// Enables DirectQuery in PowerBI Desktop against a Trino cluster. Overrides
// SQL generation defaults so PowerBI emits Trino-compatible SQL: LIMIT/OFFSET
// instead of TOP N, double-quote identifiers instead of square brackets, and
// CAST instead of CONVERT.
//
[Version = "0.0.1"]
section StackableTrinoODBC;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

// The ODBC driver name as registered in odbcinst.ini / the Windows registry.
Config_DriverName = "stackable_odbc_trino";

// Connection-string keys reachable through the `options` record.
//
// Every name is a key that src/backend/types/connect_params.rs accepts;
// `connector_options_are_connection_string_keys` in src/lib.rs fails the build
// if one is not, because an option the driver does not recognise is discarded
// at connect and reads as the setting having no effect.
//
// The four keys the driver declares sensitive through
// `Backend::sensitive_connect_keywords` are absent deliberately: an option set
// here is stored in the query text inside the .pbix, which is a file people
// mail to each other. AccessToken, ExtraCredentials, ExtraHeaders and
// ProxyPassword therefore stay on the connection string or the data source,
// where a credential store can hold them.
Config_AdvancedOptions = {
    "Source", "ClientTags", "Path", "TimeZone", "Locale", "ClientInfo",
    "TraceToken", "SessionUser", "Roles",
    "TlsVerify", "Certificate", "ClientCertificate",
    "SessionProperties", "ResourceEstimates", "ClientCapabilities", "Encoding",
    "Proxy", "ProxyUser",
    "QueryTimeout", "DisableCompression", "MaxAttempts",
    "ExternalAuthentication", "ExternalAuthenticationTimeout"
};

// SQL conformance override.
//
// The driver reports SQL_SC_SQL92_FULL here and 0 through SQLGetInfo, and
// both are right for their own audience. To an ODBC application the info type
// is a conformance claim, and Trino does not reach even Entry level: its
// CREATE TABLE rejects PRIMARY KEY, UNIQUE, CHECK and REFERENCES, and the
// driver reports SQL_TC_NONE where Entry level requires COMMIT/ROLLBACK. To
// Power Query it is the knob that unlocks SQL generation, and Microsoft's own
// guidance is to turn it up and narrow the specifics elsewhere: "In Power
// Query scenarios, the connector is used in a Read Only mode. Most drivers
// will want to report a SQL_SC_SQL92_FULL compliance level, and override
// specific SQL generation behavior using the SQLGetInfo and SQLGetFunctions
// properties."
//
// SupportsDerivedTable also keys off this: it defaults to false for any
// conformance level below SQL_SC_SQL92_FULL, and derived tables are required
// for many DirectQuery scenarios. It is set explicitly below regardless.
//
// https://learn.microsoft.com/en-us/power-query/odbc-parameters
Config_SqlConformance = ODBC[SQL_SC][SQL_SC_SQL92_FULL]; // 8

// Trino uses LIMIT x OFFSET y syntax.
Config_LimitClauseKind = LimitClauseKind.LimitOffset;

// Let Power Query bind parameters rather than inlining literals.
//
// This was `false` while the driver's parameter binding was incomplete. It is
// not any more: the declared `ParameterType` is honoured, so a numeric
// delivered as characters reaches Trino as a decimal instead of a string, and
// an unbound marker is reported rather than silently substituted.
//
// Setting it back to `false` re-adds `SQL_API_SQLBINDPARAMETER = false`, which
// contradicts the driver — `get_functions` declares `BindParameter` — and this
// file's rule is that an override is for what the driver gets *wrong*. It also
// puts every constant back through the `Constant` visitor, the part that had
// been keyed on PostgreSQL type names and folded nothing.
Config_UseParameterBindings = true;

// Trino uses single-quote escaping for string literals.
Config_StringLiteralEscapeCharacters = { "'" };

// Trino uses CAST, not CONVERT.
Config_UseCastInsteadOfConvert = true;

// Enable DirectQuery mode.
Config_EnableDirectQuery = true;

// ---------------------------------------------------------------------------
// Data source function
// ---------------------------------------------------------------------------

// The published function is the implementation re-typed, so the Get Data
// dialog renders a caption per parameter and an "Advanced options" section
// rather than asking for a bare record.
[DataSource.Kind = "StackableTrinoODBC", Publish = "StackableTrinoODBC.Publish"]
shared StackableTrinoODBC.Contents =
    Value.ReplaceType(StackableTrinoODBCImpl, StackableTrinoODBC.ContentsType);

StackableTrinoODBC.ContentsType = type function (
    server as (type text meta [
        Documentation.FieldCaption = "Server",
        Documentation.SampleValues = {"trino.example.com"}
    ]),
    port as (type number meta [
        Documentation.FieldCaption = "Port",
        Documentation.SampleValues = {8443}
    ]),
    catalog as (type text meta [
        Documentation.FieldCaption = "Catalog",
        Documentation.SampleValues = {"hive"}
    ]),
    optional schema as (type text meta [
        Documentation.FieldCaption = "Schema"
    ]),
    optional user as (type text meta [
        Documentation.FieldCaption = "User",
        Documentation.FieldDescription =
            "Left empty under external authentication, where the identity provider supplies it."
    ]),
    optional protocol as (type text meta [
        Documentation.FieldCaption = "Protocol",
        Documentation.AllowedValues = {"https", "http"}
    ]),
    optional options as (type StackableTrinoODBC.OptionsType meta [
        Documentation.FieldCaption = "Advanced options"
    ])
) as table meta [
    Documentation.Name = "Stackable Trino"
];

// Keep in step with Config_AdvancedOptions. That list is what the connection
// string is built from and what the build checks against the driver's parser;
// this type is only what the dialog renders.
StackableTrinoODBC.OptionsType = type [
    optional Source = (type text meta [Documentation.FieldCaption = "Source"]),
    optional ClientTags = (type text meta [Documentation.FieldCaption = "Client tags"]),
    optional Path = (type text meta [Documentation.FieldCaption = "SQL path"]),
    optional TimeZone = (type text meta [
        Documentation.FieldCaption = "Time zone",
        Documentation.SampleValues = {"Europe/Berlin"}
    ]),
    optional Locale = (type text meta [Documentation.FieldCaption = "Locale"]),
    optional ClientInfo = (type text meta [Documentation.FieldCaption = "Client info"]),
    optional TraceToken = (type text meta [Documentation.FieldCaption = "Trace token"]),
    optional SessionUser = (type text meta [Documentation.FieldCaption = "Session user"]),
    optional Roles = (type text meta [
        Documentation.FieldCaption = "Roles",
        Documentation.SampleValues = {"hive:ROLE{admin}"}
    ]),
    optional TlsVerify = (type text meta [
        Documentation.FieldCaption = "TLS verification",
        Documentation.AllowedValues = {"full", "ca", "none"}
    ]),
    optional Certificate = (type text meta [Documentation.FieldCaption = "CA certificate"]),
    optional ClientCertificate = (type text meta [
        Documentation.FieldCaption = "Client certificate"
    ]),
    optional SessionProperties = (type text meta [
        Documentation.FieldCaption = "Session properties",
        Documentation.SampleValues = {"query_max_run_time:10m"}
    ]),
    optional ResourceEstimates = (type text meta [
        Documentation.FieldCaption = "Resource estimates"
    ]),
    optional ClientCapabilities = (type text meta [
        Documentation.FieldCaption = "Client capabilities"
    ]),
    optional Encoding = (type text meta [
        Documentation.FieldCaption = "Spooling encoding",
        Documentation.AllowedValues = {"json", "json+zstd", "json+lz4"}
    ]),
    optional Proxy = (type text meta [Documentation.FieldCaption = "Proxy URL"]),
    optional ProxyUser = (type text meta [Documentation.FieldCaption = "Proxy user"]),
    optional QueryTimeout = (type number meta [
        Documentation.FieldCaption = "Query timeout (s)"
    ]),
    optional DisableCompression = (type logical meta [
        Documentation.FieldCaption = "Disable compression"
    ]),
    optional MaxAttempts = (type number meta [Documentation.FieldCaption = "Max attempts"]),
    optional ExternalAuthentication = (type logical meta [
        Documentation.FieldCaption = "External authentication"
    ]),
    optional ExternalAuthenticationTimeout = (type number meta [
        Documentation.FieldCaption = "External auth timeout (s)"
    ])
];

StackableTrinoODBCImpl = (
    server as text,
    port as number,
    catalog as text,
    optional schema as text,
    optional user as text,
    optional protocol as text,
    optional options as record
) as table =>
    let
        // Matches the driver's own default. An unencrypted connection should
        // be something the user chose, not what they got by leaving the
        // optional protocol argument out; a plaintext coordinator takes
        // protocol = "http".
        effectiveProtocol = if protocol <> null then protocol else "https",
        effectiveUser = if user <> null then user else "trino",

        ConnectionString = [
            Driver = Config_DriverName,
            Host = server,
            Port = Number.ToText(port),
            Catalog = catalog,
            Protocol = effectiveProtocol
        ],

        // Schema is optional in the connection string.
        ConnectionStringWithSchema =
            if schema <> null then
                ConnectionString & [Schema = schema]
            else
                ConnectionString,

        // An unrecognised option is an error rather than a value quietly
        // dropped: a misspelled key would otherwise change nothing, and a
        // report whose TimeZone silently did not apply still renders.
        suppliedOptions = if options <> null then options else [],
        unknownOptions =
            List.Difference(Record.FieldNames(suppliedOptions), Config_AdvancedOptions),
        checkedOptions =
            if List.Count(unknownOptions) > 0 then
                error "Unknown option: " & Text.Combine(unknownOptions, ", ")
            else
                suppliedOptions,

        // A connection string carries text. Numbers and logicals reach the
        // driver as "30" and "TRUE", both of which its parser accepts.
        optionsAsText = Record.FromList(
            List.Transform(
                Record.FieldValues(checkedOptions),
                each if _ is text then _ else Text.From(_)
            ),
            Record.FieldNames(checkedOptions)
        ),

        ConnectionStringWithOptions = ConnectionStringWithSchema & optionsAsText,

        // Credentials: PowerBI supplies username/password via the
        // credential system. Map them to the ODBC connection string.
        Credential = Extension.CurrentCredential(),
        CredentialConnectionString =
            if Credential[AuthenticationKind]? = "UsernamePassword" then
                [User = Credential[Username], Password = Credential[Password]]
            else
                [User = effectiveUser],

        // Build ODBC options with Trino-specific overrides.
        defaultConfig = BuildOdbcConfig(),

        SqlCapabilities = defaultConfig[SqlCapabilities] & [
            FractionalSecondsScale = 3,
            // GroupByCapabilities has no entry here, because it is not a
            // documented SqlCapabilities field. The GROUP BY relationship
            // belongs in the SQLGetInfo record instead, under SQL_GROUP_BY,
            // which the driver answers with SQL_GB_GROUP_BY_CONTAINS_SELECT.
            // That value is also the accurate one: Trino rejects a
            // non-aggregated select column absent from GROUP BY with
            // EXPRESSION_NOT_AGGREGATE, so it does not support GROUP BY
            // without restrictions.
            SupportsDerivedTable = true,
            SupportsTop = false
        ],

        // Nothing is overridden. An override here silently wins over
        // SQLGetInfoW and cannot be corrected by fixing the driver, so the
        // record is reserved for what the driver gets wrong, and this group it
        // answers honestly: SQL_SQL92_PREDICATES, SQL_AGGREGATE_FUNCTIONS,
        // SQL_SQL92_RELATIONAL_JOIN_OPERATORS, SQL_SQL92_VALUE_EXPRESSIONS and
        // SQL_IDENTIFIER_QUOTE_CHAR.
        //
        // Two of them must never be overridden with a flat value. The driver
        // gates SQL_SQL92_PREDICATES and SQL_SQL92_RELATIONAL_JOIN_OPERATORS on
        // the coordinator's version, because MATCH and UNIQUE arrived in Trino
        // 482, OVERLAPS in 483 and CORRESPONDING in 475. A flat 0x3FFF or 0x3FF
        // asserts all of them on every server, and 0x3FF also claims NATURAL
        // JOIN, which a live 467 rejects with "NOT_SUPPORTED: Natural join not
        // supported", and UNION JOIN, which has no production in Trino's
        // grammar at any version. Folding any of those produces SQL the
        // coordinator refuses.
        //
        // Nothing Power Query actually generates is lost: comparison, IN,
        // LIKE, BETWEEN, IS NULL, EXISTS and the four join types are all in
        // the driver's ungated set.
        SQLGetInfo = defaultConfig[SQLGetInfo],

        SQLColumns = (catalogName, schemaName, tableName, columnName, source) =>
            source,

        // The AstVisitor tells Power Query how to emit row limiting; without
        // it, folding fails even though LimitClauseKind.LimitOffset is set in
        // SqlCapabilities.
        //
        // Trino's grammar is `OFFSET count LIMIT count`, in that order.
        // `LIMIT 2 OFFSET 2` is rejected outright with "mismatched input
        // 'OFFSET'". Only a fold carrying both a skip and a take emits the
        // pair, so take-only folding never exposes the ordering.
        // `test_folding_contract.py` runs whatever this builds.
        AstVisitor = [
            LimitClause = (skip, take) =>
                let
                    offset = if skip <> null and skip > 0 then Text.Format("OFFSET #{0}", {skip}) else "",
                    separator = if offset <> "" and take <> null then " " else "",
                    limit = if take <> null then Text.Format("LIMIT #{0}", {take}) else ""
                in
                    [
                        Text = offset & separator & limit,
                        Location = "AfterQuerySpecification"
                    ],
            // Tell Power Query how to emit CAST expressions for typed
            // literals. Required for aggregation folding on non-SQL-Server
            // databases (from the PostgreSQL DirectQuery reference connector).
            //
            // Each record field is looked up by `typeInfo[TYPE_NAME]`, which is
            // this driver's own SQLGetTypeInfo output — so the field names are
            // Trino type names, not the reference connector's PostgreSQL ones.
            // `TEXT`, `TIMESTAMPTZ`, `TIMETZ`, `NUMERIC` and `FLOAT` came from
            // that connector and matched nothing here, so a string, timestamp
            // or time constant folded through none of them.
            // `test_folding_contract.py` asserts every field name against
            // SQLGetTypeInfo and every cast target against Trino.
            Constant =
                let
                    Quote = each Text.Format("'#{0}'", { _ }),
                    Cast = (value, typeName) => [
                        Text = Text.Format("CAST(#{0} as #{1})", { value, typeName })
                    ],
                    // An entry is added only where the cast is unambiguous.
                    // A wrong one is worse than an absent one: absent, Power
                    // Query evaluates the constant locally and the answer is
                    // still right, only unfolded. The types deliberately left
                    // out, each measured against a live coordinator:
                    //
                    //   CHAR      `CAST('abc' AS CHAR)` is char(1), not
                    //             char(3): it truncates to 'a', and
                    //             `CAST('abc' AS CHAR) = CAST('abc' AS
                    //             CHAR(3))` is false. A CHAR entry would fold
                    //             an equality filter into one matching no
                    //             rows. Trino has no way to spell the width
                    //             here, and `varchar = char(n)` already
                    //             compares true, so there is nothing to gain.
                    //   INTERVAL DAY TO SECOND, INTERVAL YEAR TO MONTH
                    //             Trino cannot cast varchar to either:
                    //             "Cannot cast varchar(10) to interval day to
                    //             second". No CAST target exists to name.
                    //   VARBINARY The constant is an M binary value, and
                    //             rendering it as Trino's `X'..'` literal is
                    //             not something this can be checked to get
                    //             right without Power BI Desktop in the loop.
                    //   UUID, JSON, TIME/TIMESTAMP WITH TIME ZONE
                    //             Each has a valid Trino cast, but the
                    //             rendering turns on whether Power Query
                    //             hands a text-valued constant to the visitor
                    //             already quoted. That is unresolved, and is
                    //             the same question the VARCHAR entry below
                    //             rests on.
                    //             Settle that first, in Power BI Desktop, and
                    //             all five can be added together.
                    //
                    // Microsoft documents this whole override as deprecated
                    // ("Providing an override for the Constant value within
                    // AstVisitor has been deprecated and may be removed in
                    // future implementations"), so the list below is kept to
                    // what pays for itself rather than grown to cover every
                    // TYPE_NAME the driver reports.
                    Visitor = [
                        DECIMAL = each Cast(_, "DECIMAL"),
                        INTEGER = each Cast(_, "INTEGER"),
                        BIGINT = each Cast(_, "BIGINT"),
                        SMALLINT = each Cast(_, "SMALLINT"),
                        TINYINT = each Cast(_, "TINYINT"),
                        REAL = each Cast(_, "REAL"),
                        DOUBLE = each Cast(_, "DOUBLE PRECISION"),
                        BOOLEAN = each Cast(_, "BOOLEAN"),
                        DATE = each Cast(Quote(Date.ToText(_, "yyyy-MM-dd")), "DATE"),
                        VARCHAR = each Cast(_, "VARCHAR"),
                        TIMESTAMP = each Cast(Quote(DateTime.ToText(_, "yyyy-MM-dd HH:mm:ss.sssssss")), "TIMESTAMP"),
                        TIME = each Cast(Quote(Time.ToText(_, "HH:mm:ss.sssssss")), "TIME")
                    ]
                in
                    (typeInfo, ast) => Record.FieldOrDefault(Visitor, typeInfo[TYPE_NAME], each null)(ast[Value])
        ],

        // Pass through SQLGetTypeInfo from the driver unchanged.
        SQLGetTypeInfo = (types) => types,

        OdbcDatasource = Odbc.DataSource(
            ConnectionStringWithOptions,
            [
                HierarchicalNavigation = true,
                SoftNumbers = true,
                TolerateConcatOverflow = true,
                ClientConnectionPooling = true,
                CredentialConnectionString = CredentialConnectionString,
                SqlCapabilities = SqlCapabilities,
                SQLGetInfo = SQLGetInfo,
                SQLColumns = SQLColumns,
                SQLGetTypeInfo = SQLGetTypeInfo,
                AstVisitor = AstVisitor
            ]
        )
    in
        OdbcDatasource;

// ---------------------------------------------------------------------------
// Data source kind
// ---------------------------------------------------------------------------

StackableTrinoODBC = [
    TestConnection = (dataSourcePath) =>
        let
            json = Json.Document(dataSourcePath),
            server = json[server],
            port = json[port],
            catalog = json[catalog]
        in
            {"StackableTrinoODBC.Contents", server, port, catalog},
    Authentication = [
        UsernamePassword = [],
        Implicit = []
    ],
    Label = Extension.LoadString("DataSourceLabel")
];

// ---------------------------------------------------------------------------
// Publishing metadata
// ---------------------------------------------------------------------------

StackableTrinoODBC.Publish = [
    Category = "Database",
    ButtonText = {Extension.LoadString("ButtonTitle"), Extension.LoadString("ButtonHelp")},
    LearnMoreUrl = "https://trino.io/",
    SupportsDirectQuery = Config_EnableDirectQuery,
    SourceImage = StackableTrinoODBC.Icons,
    SourceTypeImage = StackableTrinoODBC.Icons
];

StackableTrinoODBC.Icons = [
    Icon16 = {
        Extension.Contents("StackableTrinoODBC16.png"),
        Extension.Contents("StackableTrinoODBC20.png"),
        Extension.Contents("StackableTrinoODBC24.png"),
        Extension.Contents("StackableTrinoODBC32.png")
    },
    Icon32 = {
        Extension.Contents("StackableTrinoODBC32.png"),
        Extension.Contents("StackableTrinoODBC40.png"),
        Extension.Contents("StackableTrinoODBC48.png"),
        Extension.Contents("StackableTrinoODBC64.png")
    }
];

// ---------------------------------------------------------------------------
// ODBC configuration builder
// ---------------------------------------------------------------------------

BuildOdbcConfig = () as record =>
    let
        Merge = (previous as record, optional caps as record, optional funcs as record, optional getInfo as record) as record =>
            let
                newCaps = if (caps <> null) then previous[SqlCapabilities] & caps else previous[SqlCapabilities],
                newFuncs = if (funcs <> null) then previous[SQLGetFunctions] & funcs else previous[SQLGetFunctions],
                newGetInfo = if (getInfo <> null) then previous[SQLGetInfo] & getInfo else previous[SQLGetInfo]
            in
                [SqlCapabilities = newCaps, SQLGetFunctions = newFuncs, SQLGetInfo = newGetInfo],

        defaultConfig = [
            SqlCapabilities = [],
            SQLGetFunctions = [],
            SQLGetInfo = []
        ],

        // Parameter bindings disabled — inline literals instead.
        withParams =
            if (Config_UseParameterBindings = false) then
                let
                    caps = [
                        SupportsNumericLiterals = true,
                        SupportsStringLiterals = true,
                        SupportsOdbcDateLiterals = true,
                        SupportsOdbcTimeLiterals = true,
                        SupportsOdbcTimestampLiterals = true
                    ],
                    funcs = [
                        SQL_API_SQLBINDPARAMETER = false
                    ]
                in
                    Merge(defaultConfig, caps, funcs)
            else
                defaultConfig,

        // String literal escaping.
        withEscape =
            if (Config_StringLiteralEscapeCharacters <> null) then
                Merge(withParams, [StringLiteralEscapeCharacters = Config_StringLiteralEscapeCharacters])
            else
                withParams,

        // LIMIT/OFFSET clause style.
        withLimitClauseKind =
            Merge(withEscape, [LimitClauseKind = Config_LimitClauseKind]),

        // CAST instead of CONVERT.
        //
        // Kept even though the driver already reports SQL_FN_CVT_CAST and no
        // CONVERT, because the M engine's documented default is to attempt
        // CONVERT and this is the documented way to redirect it. The override
        // agrees with the driver rather than contradicting it.
        withCastOrConvert =
            if (Config_UseCastInsteadOfConvert <> null) then
                let
                    value =
                        if (Config_UseCastInsteadOfConvert = true) then
                            ODBC[SQL_FN_CVT][SQL_FN_CVT_CAST]
                        else
                            ODBC[SQL_FN_CVT][SQL_FN_CVT_CONVERT],
                    getInfo = [SQL_CONVERT_FUNCTIONS = value]
                in
                    Merge(withLimitClauseKind, null, null, getInfo)
            else
                withLimitClauseKind,

        // SQL conformance override.
        withSqlConformance =
            if (Config_SqlConformance <> null) then
                Merge(withCastOrConvert, null, null, [SQL_SQL_CONFORMANCE = Config_SqlConformance])
            else
                withCastOrConvert
    in
        withSqlConformance;

// ---------------------------------------------------------------------------
// Helper modules (loaded from .pqm files bundled in the .mez)
// ---------------------------------------------------------------------------

Extension.LoadFunction = (name as text) =>
    let
        binary = Extension.Contents(name),
        asText = Text.FromBinary(binary)
    in
        Expression.Evaluate(asText, #shared);

ODBC = Extension.LoadFunction("OdbcConstants.pqm");
