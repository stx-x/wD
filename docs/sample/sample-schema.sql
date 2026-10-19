-- 由 MSSQL Transfer Studio 生成
-- 数据库：SalesDB
SET ANSI_NULLS ON;
SET QUOTED_IDENTIFIER ON;
GO

IF DB_ID(N'SalesDB') IS NULL CREATE DATABASE [SalesDB];
GO
USE [SalesDB];
GO

IF OBJECT_ID(N'dbo.Customers', N'U') IS NOT NULL DROP TABLE [dbo].[Customers];
GO
CREATE TABLE [dbo].[Customers] (
    [Id] int IDENTITY(1,1) NOT NULL,
    [Name] nvarchar(50) COLLATE Chinese_PRC_CI_AS NOT NULL,
    [Balance] decimal(18,2) NULL CONSTRAINT [DF_Balance] DEFAULT ((0)),
    CONSTRAINT [PK_Customers] PRIMARY KEY CLUSTERED ([Id])
);
GO

IF OBJECT_ID(N'dbo.Orders', N'U') IS NOT NULL DROP TABLE [dbo].[Orders];
GO
CREATE TABLE [dbo].[Orders] (
    [Id] bigint IDENTITY(1,1) NOT NULL,
    [CustomerId] int NOT NULL,
    CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Id]),
    CONSTRAINT [CK_Orders_CustomerId] CHECK ([CustomerId]>(0))
);
GO

CREATE NONCLUSTERED INDEX [IX_Customers_Name] ON [dbo].[Customers] ([Name]);
GO

ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [FK_Orders_Customers] FOREIGN KEY ([CustomerId]) REFERENCES [dbo].[Customers] ([Id]) ON DELETE CASCADE;
GO

EXEC sp_addextendedproperty N'MS_Description', N'客户主表', N'SCHEMA', N'dbo', N'TABLE', N'Customers', NULL, NULL;
GO
EXEC sp_addextendedproperty N'MS_Description', N'主键', N'SCHEMA', N'dbo', N'TABLE', N'Customers', N'COLUMN', N'Id';
GO

