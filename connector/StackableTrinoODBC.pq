// Power Query custom connector for Trino via the Stackable ODBC driver.
//
// Enables DirectQuery in PowerBI Desktop against a Trino cluster. Overrides
// SQL generation defaults so PowerBI emits Trino-compatible SQL: LIMIT/OFFSET
// instead of TOP N, double-quote identifiers instead of square brackets, and
// CAST instead of CONVERT.
//
[Version = "1.0.0"]
section StackableTrinoODBC;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

// The ODBC driver name as registered in odbcinst.ini / the Windows registry.
Config_DriverName = "stackable_odbc_trino";

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

[DataSource.Kind = "StackableTrinoODBC", Publish = "StackableTrinoODBC.Publish"]
shared StackableTrinoODBC.Contents = (
    server as text,
    port as number,
    catalog as text,
    optional schema as text,
    optional user as text,
    optional protocol as text
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
            // GroupByCapabilities is not a documented SqlCapabilities field --
            // the GROUP BY relationship is expressed through SQL_GROUP_BY in
            // the SQLGetInfo record, which the driver now answers with
            // SQL_GB_GROUP_BY_CONTAINS_SELECT (the value that used to be here,
            // in the wrong record). Its old comment, "Trino supports GROUP BY
            // without restrictions", was wrong either way: Trino rejects a
            // non-aggregated select column that is absent from GROUP BY with
            // EXPRESSION_NOT_AGGREGATE.
            SupportsDerivedTable = true,
            SupportsTop = false
        ],

        // Overrides are for what the driver gets wrong. Anything the driver
        // reports correctly is deliberately absent, because an override here
        // silently wins over SQLGetInfoW and cannot be corrected by fixing the
        // driver. SQL_SQL92_PREDICATES, SQL_AGGREGATE_FUNCTIONS,
        // SQL_SQL92_RELATIONAL_JOIN_OPERATORS, SQL_SQL92_VALUE_EXPRESSIONS and
        // SQL_IDENTIFIER_QUOTE_CHAR were all overridden here and are all now
        // answered honestly by the driver.
        //
        // Two of them were actively harmful. The driver gates
        // SQL_SQL92_PREDICATES and SQL_SQL92_RELATIONAL_JOIN_OPERATORS on the
        // coordinator's version, because MATCH and UNIQUE arrived in Trino
        // 482, OVERLAPS in 483 and CORRESPONDING in 475. The flat 0x3FFF and
        // 0x3FF here re-asserted all of them on every server, and 0x3FF also
        // claimed NATURAL JOIN -- which a live 467 rejects with
        // "NOT_SUPPORTED: Natural join not supported" -- and UNION JOIN, which
        // has no production in Trino's grammar at any version. Folding any of
        // those produces SQL the coordinator refuses.
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
        // Trino's grammar is `OFFSET count LIMIT count`, in that order --
        // `LIMIT 2 OFFSET 2` is rejected outright with "mismatched input
        // 'OFFSET'". Only a fold carrying both a skip and a take emits the
        // pair, which is why take-only folding worked regardless.
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
                    Visitor = [
                        DECIMAL = each Cast(_, "DECIMAL"),
                        INTEGER = each Cast(_, "INTEGER"),
                        REAL = each Cast(_, "REAL"),
                        DOUBLE = each Cast(_, "DOUBLE PRECISION"),
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
            ConnectionStringWithSchema,
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
