# 数据库结构上下文：SalesDB

> 本文档由 MSSQL Transfer Studio 生成，用于向 AI / Agent 描述数据库结构。
> 请基于以下结构编写 SQL，不要臆造不存在的表或列。

## 1. 数据库概览

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

## 2. 对象清单

| 对象 | 类型 | 行数 | 列数 | 主键 | 说明 |
|---|---|---|---|---|---|
| `dbo.Customers` | 表 | 12,480 | 3 | Id | 客户主表 |
| `dbo.Orders` | 表 | 89,301 | 2 | Id |  |

## 3. 关系（外键）

- `dbo.Orders`(CustomerId) → `dbo.Customers`(Id)  ON DELETE CASCADE

## 4. 表结构详解

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

---

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

---

## 5. 生成 SQL 的注意事项

- **IDENTITY 列**（不要显式插入值，除非 `SET IDENTITY_INSERT ON`）：dbo.Customers.Id、dbo.Orders.Id
- **无默认值的 NOT NULL 列**（INSERT 必须提供）：dbo.Customers.Name、dbo.Orders.CustomerId
- **精确小数列**（请使用精确字面量，不要经过浮点运算）：dbo.Customers.Balance decimal(18,2)
- 方言为 **Microsoft SQL Server (T-SQL)**：分页用 `ORDER BY ... OFFSET n ROWS FETCH NEXT m ROWS ONLY` 或 `TOP (n)`；字符串拼接用 `+`；日期字面量写成 `'YYYY-MM-DD'`。
- 标识符用方括号包裹（如 `[dbo].[Orders]`），字符串用单引号，`N'...'` 表示 Unicode。
- 判断 NULL 用 `IS NULL` / `IS NOT NULL`，不要用 `= NULL`。
