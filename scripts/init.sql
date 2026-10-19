-- MSSQL Transfer Studio — 测试用示例库
-- 覆盖 int / bigint / decimal / datetime2 / nvarchar / varbinary / bit / NULL

IF DB_ID(N'TransferDemo') IS NULL
    CREATE DATABASE TransferDemo;
GO
USE TransferDemo;
GO

IF OBJECT_ID(N'dbo.Customers', N'U') IS NOT NULL DROP TABLE dbo.Customers;
IF OBJECT_ID(N'dbo.Orders', N'U') IS NOT NULL DROP TABLE dbo.Orders;
IF OBJECT_ID(N'dbo.Products', N'U') IS NOT NULL DROP TABLE dbo.Products;
GO

CREATE TABLE dbo.Customers (
    Id          int IDENTITY(1,1) NOT NULL,
    Name        nvarchar(100)     NOT NULL,
    Email       nvarchar(200)     NULL,
    Balance     decimal(18,2)     NOT NULL DEFAULT 0,
    IsActive    bit               NOT NULL DEFAULT 1,
    CreatedAt   datetime2(3)      NOT NULL DEFAULT SYSUTCDATETIME(),
    Avatar      varbinary(max)    NULL,
    CONSTRAINT PK_Customers PRIMARY KEY (Id)
);

CREATE TABLE dbo.Products (
    Sku         varchar(32)       NOT NULL,
    Title       nvarchar(200)     NOT NULL,
    Price       money             NOT NULL,
    Stock       int               NOT NULL,
    CONSTRAINT PK_Products PRIMARY KEY (Sku)
);

CREATE TABLE dbo.Orders (
    Id          bigint IDENTITY(1,1) NOT NULL,
    CustomerId  int               NOT NULL,
    Sku         varchar(32)       NOT NULL,
    Qty         smallint          NOT NULL,
    Amount      decimal(18,2)     NOT NULL,
    Note        nvarchar(max)     NULL,
    OrderedAt   datetime2(7)      NOT NULL,
    CONSTRAINT PK_Orders PRIMARY KEY (Id)
);
GO

-- 特殊字符：单引号、中文、换行、NULL
INSERT INTO dbo.Customers (Name, Email, Balance, IsActive, Avatar) VALUES
 (N'张伟',   N'zhang@example.com', 1280.50, 1, 0x89504E470D0A1A0A),
 (N'李娜',   N'li@example.com',       0.00, 0, NULL),
 (N'O''Brien', N'obrien@example.com', -42.75, 1, NULL),
 (N'王强',   NULL,                  9999.99, 1, 0x00FF);

INSERT INTO dbo.Products (Sku, Title, Price, Stock) VALUES
 ('SKU-001', N'机械键盘',   399.00, 120),
 ('SKU-002', N'显示器 27"', 1899.00, 35),
 ('SKU-003', N'USB-C 线缆',   39.90, 800);

INSERT INTO dbo.Orders (CustomerId, Sku, Qty, Amount, Note, OrderedAt) VALUES
 (1, 'SKU-001', 2,  798.00, N'加急',            '2024-03-11 09:12:00.123'),
 (1, 'SKU-003', 5,  199.50, NULL,               '2024-03-11 09:12:00.000'),
 (2, 'SKU-002', 1, 1899.00, N'含运费
第二行备注', '2024-03-12 14:05:31.500');
GO

CREATE VIEW dbo.vOrderSummary AS
SELECT c.Name, COUNT(o.Id) AS OrderCount, ISNULL(SUM(o.Amount), 0) AS Total
FROM dbo.Customers c
LEFT JOIN dbo.Orders o ON o.CustomerId = c.Id
GROUP BY c.Name;
GO

SELECT 'Customers' AS tbl, COUNT(*) AS rows FROM dbo.Customers
UNION ALL SELECT 'Products', COUNT(*) FROM dbo.Products
UNION ALL SELECT 'Orders', COUNT(*) FROM dbo.Orders;
GO
