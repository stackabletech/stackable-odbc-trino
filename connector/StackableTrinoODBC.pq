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

// SQL conformance override. The driver currently reports SQL_SC_MINIMUM (1),
// but it supports the SQL-92 Entry Level constructs PowerBI needs. Override
// to SQL_SC_SQL92_FULL (8) so query folding isn't artificially limited.
Config_SqlConformance = ODBC[SQL_SC][SQL_SC_SQL92_FULL]; // 8

// Trino uses LIMIT x OFFSET y syntax.
Config_LimitClauseKind = LimitClauseKind.LimitOffset;

// Parameter bindings are not yet fully supported. Inline literals instead.
Config_UseParameterBindings = false;

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
        effectiveProtocol = if protocol <> null then protocol else "http",
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
            // Trino supports GROUP BY without restrictions.
            GroupByCapabilities = 2,
            SupportsDerivedTable = true,
            SupportsTop = false
        ],

        SQLGetInfo = defaultConfig[SQLGetInfo] & [
            // Override SQL-92 capabilities that the driver doesn't yet
            // report correctly. Power Query checks these via ODBC
            // SQLGetInfoW and refuses to fold if they're zero.
            //
            // 0x3FFF = all SQL_SP bits (EXISTS, ISNOTNULL, ISNULL, LIKE, IN, BETWEEN, etc.)
            SQL_SQL92_PREDICATES = 0x3FFF,
            // 0x7F = AVG | COUNT | MAX | MIN | SUM | DISTINCT | ALL
            SQL_AGGREGATE_FUNCTIONS = 0x7F,
            // CROSS | FULL OUTER | INNER | LEFT OUTER | NATURAL | RIGHT OUTER + more
            SQL_SQL92_RELATIONAL_JOIN_OPERATORS = 0x3FF,
            // CASE | CAST | COALESCE | NULLIF
            SQL_SQL92_VALUE_EXPRESSIONS = 0x0F,
            // Tell PowerBI to use double-quote for identifier quoting
            // instead of SQL Server-style square brackets.
            SQL_IDENTIFIER_QUOTE_CHAR = """"
        ],

        SQLColumns = (catalogName, schemaName, tableName, columnName, source) =>
            source,

        // Trino uses LIMIT x OFFSET y — the AstVisitor tells Power Query
        // how to emit this syntax. Without it, query folding fails even
        // though LimitClauseKind.LimitOffset is set in SqlCapabilities.
        AstVisitor = [
            LimitClause = (skip, take) =>
                let
                    limit = if take <> null then Text.Format("LIMIT #{0}", {take}) else "",
                    offset = if skip <> null and skip > 0 then Text.Format(" OFFSET #{0}", {skip}) else ""
                in
                    [
                        Text = limit & offset,
                        Location = "AfterQuerySpecification"
                    ],
            // Tell Power Query how to emit CAST expressions for typed
            // literals. Required for aggregation folding on non-SQL-Server
            // databases (from the PostgreSQL DirectQuery reference connector).
            Constant =
                let
                    Quote = each Text.Format("'#{0}'", { _ }),
                    Cast = (value, typeName) => [
                        Text = Text.Format("CAST(#{0} as #{1})", { value, typeName })
                    ],
                    Visitor = [
                        NUMERIC = each Cast(_, "NUMERIC"),
                        DECIMAL = each Cast(_, "DECIMAL"),
                        INTEGER = each Cast(_, "INTEGER"),
                        FLOAT = each Cast(_, "FLOAT"),
                        REAL = each Cast(_, "REAL"),
                        DOUBLE = each Cast(_, "DOUBLE PRECISION"),
                        DATE = each Cast(Quote(Date.ToText(_, "yyyy-MM-dd")), "DATE"),
                        TEXT = each Cast(_, "VARCHAR"),
                        TIMESTAMPTZ = each Cast(Quote(DateTime.ToText(_, "yyyy-MM-dd HH:mm:ss.sssssss")), "TIMESTAMP"),
                        TIMETZ = each Cast(Quote(Time.ToText(_, "HH:mm:ss.sssssss")), "TIME")
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
