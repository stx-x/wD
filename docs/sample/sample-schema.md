# 数据库结构：SalesDB

## 概览

| 项 | 值 |
|---|---|
| 数据库 | `SalesDB` |
| SQL Server 版本 | 16.0.4135 Enterprise |
| 排序规则 | Chinese_PRC_CI_AS |
| 兼容级别 | 160 |
| 恢复模式 | FULL |
| 表 | 2 个 |
| 视图 | 0 个 |
| 存储过程 / 函数 | 0 个 |
| 数据总行数（估算） | 101,781 |

## 对象清单

| 对象 | 类型 | 行数 | 列数 | 主键 | 说明 |
|---|---|---|---|---|---|
| `dbo.Customers` | 表 | 12,480 | 3 | Id | 客户主表 |
| `dbo.Orders` | 表 | 89,301 | 2 | Id |  |

## 表结构

### `dbo.Customers`

行数约 12,480 · 3 列 · 主键 (Id)

客户主表

| 列 | 类型 | 可空 | 键 | 默认值 | 说明 |
|---|---|---|---|---|---|
| `Id` | int | 否 | PK IDENTITY | — | 主键 |
| `Name` | nvarchar(50) | 否 | — | — |  |
| `Balance` | decimal(18,2) | 是 | — | ((0)) |  |

**索引**

- `IX_Customers_Name` (Name)

**样例数据（前 1 行）**

| Id | Name | Balance |
|---|---|---|
| 1 | 张伟 | 0.00 |

### `dbo.Orders`

行数约 89,301 · 2 列 · 主键 (Id)

| 列 | 类型 | 可空 | 键 | 默认值 | 说明 |
|---|---|---|---|---|---|
| `Id` | bigint | 否 | PK IDENTITY | — |  |
| `CustomerId` | int | 否 | — | — |  |

**外键**

- `FK_Orders_Customers` (CustomerId) → `dbo.Customers` (Id)

**检查约束**

- `CK_Orders_CustomerId`: ([CustomerId]>(0))

